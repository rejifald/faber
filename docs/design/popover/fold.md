# Tray popover, folded lists that open in place: anatomy

Reference: [`./reference/fold-*.png`](reference/) (frozen 2026-10-31; each artboard in `-light`
and `-dark`) · Decided: 2026-10-31 by the owner, from rendered options · Extends
[`anatomy.md`](anatomy.md) § 5 and § 6, which this file overrides where they disagree.

The popover folds two lists: the attention list past three rows, and the jobs column past the
gauge column's height. Both used to end in one `MoreRow`, "+N more … in the panel", that opened
the panel window. This pass replaces that row in both places. A builder reads this file, the
reference images, and `anatomy.md` for everything around them.

## 1. The screen

- **Purpose:** the folded line answers "is anything I can't see worth a look?" without leaving
  the popover, and lets the operator look.
- **Data:** the folded job groups (each one's worst pip, `worstPip` in
  `ui/src/popover/groups.ts`), and the folded attention rows (each one's `severity`).
- **Viewport:** the 620 px card (jobs column 380 px) and the 308 px card (attention list only);
  light and dark.
- **Sparsest data:** two folded attention rows of one severity; one compact line and one folded
  group. **Densest:** 20+ groups with every pip kind folded.

## 2. Decisions

All by the owner on 2026-10-31.

- **The folded line opens in place.** "Show N more" is a disclosure: a chevron that turns, as a
  gate or job group does. Open, it reads "Show fewer". The panel stays reachable: "Open panel ↗"
  sits beside "Show fewer" in the jobs column, and the ⋯ menu's *Open panel* is unchanged.
- **The collapsed line says what it hides:** one count per kind present. Jobs: one still pip per
  pip kind, in urgency order (runner failed, frozen, waiting, running, done), each
  with its count of folded groups whose worst pip is that kind. Attention: one icon per severity
  (bad, warn, info), each with its count.
- **Folded groups go worst first.** Past the groups shown in full, groups sort by their worst
  pip's urgency (stable, so equal groups keep the oldest-first order), so a runner failure takes the
  first compact line and is never only a count. Groups shown in full keep today's order. The
  attention list is already worst first (`sortRows`), so it needs no change.
- **Opening never makes the jobs column taller.** Open, the whole jobs column (full groups and
  every folded group as a compact line) scrolls at the height it folded at, with a fade at an
  edge that has more beyond it. "Show fewer" and "Open panel ↗" sit under the scroll area.
- **Opening the attention list makes the card taller**, since its rows carry actions, and buttons
  do not belong in a scroll area. If the card would pass the window's cap (`maxHeight`), the
  attention rows scroll inside the card instead (owner, 2026-10-31).
- **Both close when the popover closes** (owner, 2026-10-31): each open starts at the glance
  view. Open gates and job groups keep their current behaviour.
- **Look:** the line has no fill, no radius and no accent. It is a row on the same grid as a
  compact line (jobs) or an attention row's padding (attention), `text-small`, `text-muted`, with
  the row-hover and row-press tints.

- **The tally's counts are `text-muted`; only its marks carry colour** (owner, 2026-10-31,
  #450 design review). The attention tally draws red and amber issues with the same
  `TriangleAlert` the rows use, told apart by tone, as the rows are; its accessible name says
  "problem" and "warning" in words.

*Rejected* (2026-10-31), so review does not reopen them: the old pill that opened the panel;
a tally that only opens the panel (direction A); dropping compact lines for a plain scroll
(direction C, which also reopened the 2026-10-25 compact-line decision).

## 3. Code archaeology

- `MoreRow` is in `ui/src/blocks/AttentionRow.tsx` (bottom). Used twice in
  `ui/src/screens/Popover.tsx`: after the attention rows (`hidden`, from `foldRows`) and after the
  compact lines (`rest`, `noun="groups"`), both calling `showPanel`.
- `fitJobs` (`ui/src/popover/wiring.ts`) sizes the fold from the measured `[data-more]` row
  (`MORE_ROW_GUESS` before it is drawn). The new collapsed line keeps `data-more`, so the fold
  maths is unchanged; its height must stay one row, so it never wraps.
- `rowHeights` remembers the compact and more rows' heights because a guess that differs from
  the real height made the fold flip between two counts (test "the fold settles…"). Keep it.
- `buildGroups` orders live groups by oldest job, then finished groups by newest. The fold's
  worst-first order applies only to `groups.slice(full)`; it must not reorder `groupKeys`, which
  `fitJobs` reads with the heights remembered by key.
- `URGENCY` in `groups.ts` is the order `worstPip` uses; the tally and the fold order reuse it
  (export it, do not copy it).
- The popover hides on blur (`wireWindow` in `wiring.ts` shuts the ⋯ menu then). "Close when the
  popover closes" means: collapse both on the window's `blur`.
- `PipStrip` takes `still` (no spin, no fade): the tally's pips are drawn still.
- `foldRows` shows a fourth attention row in place of "+1 more", so the attention line always
  hides two or more. Keep that.

## 4. Component inventory

| Element | Component | Verdict | Import | Fidelity | Artboards |
| --- | --- | --- | --- | --- | --- |
| Collapsed line, jobs | `FoldToggle` (replaces `MoreRow`) | new | `ui/src/blocks/FoldToggle.tsx` | as drawn | `fold-jobs-collapsed`, `fold-jobs-failure` |
| Collapsed line, attention | `FoldToggle` | new | same | as drawn | `fold-attention-collapsed` |
| Open footer, jobs ("Show fewer", "Open panel ↗") | `FoldToggle` open, with `onPanel` | new | same | as drawn | `fold-jobs-open` |
| Open footer, attention ("Show fewer") | `FoldToggle` open | new | same | as drawn | `fold-attention-open` |
| The pressable line itself ("Show N more", "Show fewer", "Open panel") | `RowButton`: full-bleed one-line row button (added in the build: lint bans a raw `<button>` outside the kit, and nothing in the kit was a row button) | new | `ui/src/kit/RowButton.tsx` | as drawn | every fold artboard |
| Tally pip | `PipStrip` with `still` | reuse | `ui/src/blocks/PipStrip.tsx` | as drawn | jobs artboards |
| Tally severity icon | `Icon` + lucide `TriangleAlert` / `Info`, tone as `AttentionRow`'s `MARK` | reuse | `ui/src/kit` | as drawn | attention artboards |
| Compact line | `CompactGroup` | reuse | `ui/src/blocks/CompactGroup.tsx` | as drawn | jobs artboards |
| Jobs scroll area with edge fades | in `Popover.tsx` | new (local) | — | as drawn | `fold-jobs-open` |
| Attention row | `AttentionRow` | reuse | `ui/src/blocks/AttentionRow.tsx` | as drawn | attention artboards |

`MoreRow` is removed; nothing else uses it.

## 5. FoldToggle (`new`)

- **What:** the last line of a list the popover folds. Collapsed: chevron, "Show N more", then a
  tally of what is folded. Open: chevron turned up, "Show fewer", and, when given, "Open panel ↗"
  at the right.
- **Props:** `count: number` (folded items); `open: boolean`; `onOpenChange(open)`;
  `tally: { key: string; mark: ReactNode; count: number; label: string }[]` (in the order drawn;
  `label` is the words, e.g. "running", "warning"); `onPanel?: () => void` (open state only);
  `controls?: string` (the id of the list it opens, for `aria-controls`).
- **States:** collapsed, collapsed with a runner failure folded, open, open
  with panel link, hover, press, focus-visible.
- **Layout:** one row, never wraps. Collapsed: the grid of a compact line (`1fr auto w-age`,
  `px-xl`, `py-3xs`): chevron and words left, tally where pips go, the age column empty. Tally
  entries `gap-md`; each is its mark and the count in `font-mono text-caption tabular-nums
  text-muted`, `gap-2xs`. Open: "Show fewer" left, "Open panel" with `ArrowUpRight` right,
  `border-t border-border`, `px-xl pt-2xs`.
- **Tokens:** text `text-small text-muted`; chevron and ↗ `text-faint`, `text-fg` on hover;
  hover `bg-row-hover`, press `bg-row-press`; no fill at rest, no radius. Chevron `ChevronRight`
  at the `sm` icon step, turned 90° open (`ChevronUp` drawn on "Show fewer").
- **a11y:** the line is a `button` with `aria-expanded` and `aria-controls`; its accessible name
  says the tally in words: "Show 11 more: 3 running, 2 waiting, 6 passed". "Open panel" is its own
  button. Enter and Space toggle. Focus stays on the toggle when it flips.
- **Motion:** the chevron turns over the kit's disclosure duration; none under reduced motion.
- **Content:** "Show N more", "Show fewer", "Open panel". Sentence case, no "in the panel".
- **Tests:** the name carries the tally in words; toggling calls `onOpenChange`; "Open panel"
  calls `onPanel` and does not toggle.

## 6. Composition

- **Jobs column, collapsed:** full groups, the hairline, compact lines, then `FoldToggle` in the
  `[data-more]` slot (no `px-xs` inset any more: the row is full bleed like a compact line). The
  compact lines come from the worst-first order.
- **Jobs column, open:** the column is a scroll area with `max-height` = the height it had folded
  at (the gauge column's room), holding every group: the full ones, the hairline, then every other
  group as a compact line, worst first. Under it, outside the scroll, the open `FoldToggle` with
  "Open panel". A 24 px fade (`--bg` to transparent) shows at the top once scrolled and at the
  bottom while more is below. The column's height does not change when it opens.
- **Attention list, collapsed:** three rows, then `FoldToggle` with the severity tally, at
  the attention band's horizontal padding, on `--bg`, above the band's hairline.
- **Attention list, open:** every row, then the open `FoldToggle` ("Show fewer", no panel link).
  The card grows. When it would pass `maxHeight`, the attention rows (not the toggle) get a
  `max-height` and scroll.
- **Both** collapse on the window's `blur`, and when the folded set empties.

## 7. Out of scope

- The peer list's "+N more hosts" line (`PEERS_MAX`), which stays as it is.
- The panel window and the dashboard's own lists.
- The 308 px card never folds jobs (it shows no jobs), so only its attention fold changes.
- Keyboard scrolling inside the open jobs column beyond the browser's own.
