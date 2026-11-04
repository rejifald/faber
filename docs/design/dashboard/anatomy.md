# Dashboard (direction B) — anatomy

Reference: `./reference/B-Overview.png` (frozen 2026-10-28) · Decided: direction B,
nav N2, owner 2026-10-28.

Re-rendered on 2026-11-03 from the Storybook stories, with illustrative data; the frozen originals
stay with the legacy repository.

`B-Navigation.png` (frozen 2026-10-28) and `B-Overview-v1.png` (the unrefined B, kept for history
only) are no longer in this tree, since no story renders them; the originals stay with the legacy
repository. `B-Navigation.png` drew three ways to slice the daemon's one state blob: N1, as drawn
in B, eight items in two groups (Monitor: Overview, In flight, Runs, Classes, Faults, Retries;
Configure: Settings, Audit log); N2, five nouns (Overview, Activity, Memory, Repositories,
Settings) under a host card, with an update card at the foot, the one decided here; and N3, the
same five nouns as a top bar with the host as a dropdown.

Status: **step 1 (components)**, merged in #413. The screens (step 3: Overview, Activity A1,
Memory M1) are in [`screens.md`](screens.md).

## 1. The screen

- **Purpose:** faber's one full UI (ADR-0041). Answers "is this host healthy, who is using the
  memory budget, and what happened" for the operator of one host.
- **Who:** the host's operator, in the app's panel window or a browser on loopback, behind a
  session (ADR-0028 addendum C).
- **Data:** one `GET /api/state` snapshot (`StateView`, `src/reconcile.rs`), polled; settings
  from `GET /api/settings`. No per-repo roster and no time series exist yet (see §7).
- **Entry points:** the popover's ⋯ menu and ⌘⇧F (panel window), *Open dashboard*.
- **Primary viewport:** 1280 px wide (artboard). No breakpoint is defined for the dashboard
  except `narrow` (640 px), which Settings already uses.
- **Sparsest real data:** a fresh host: no jobs in flight, no learned classes, no peers, no
  history, no update pending. Every component must render that without inventing filler.

## 2. Decisions

- Direction B (sidebar, dense tables, right rail) over A and C — owner 2026-10-28: it scales.
- Nav IA N2 (five nouns, outcome types as filters inside a view) over N1 and N3 — owner
  2026-10-28.
- Built on the existing kit (ADR-0042) and theme (ADR-0038–0040). The artboards were drawn
  in raw px with system fonts before this mapping; §4's token column is the mapping.
- Rejected: building B's colours as drawn. The artboards use the popover-era dark palette as
  literals; the build uses the ADR-0037 roles only, in light and dark.

**Answered 2026-10-28 (owner):**

- **O1 → build neutral.** ADR-0041 (one page, four groups, jump-list) and N2 (five views)
  are not reconciled yet. `DashboardNav` takes items with an `href` and a `mode`: `page`
  (`aria-current="page"`) or `section` (`aria-current="location"`, jump links). The IA, and
  whether ADR-0041 § 4 is superseded, is decided at the screen step. Nothing in step 1
  hardcodes N2's labels. **Settled 2026-10-28:** N2's five views, each its own URL
  ([ADR-0054](../../adrs/0054-the-dashboard-is-views-in-a-sidebar.md)); the shell uses
  `page` mode.
- **O2 → status card + peer menu.** Diverges from the reference: the ⇅ chevron that promises
  switching → a menu trigger opening a read-only list of peers (name, runners, last seen) —
  the dashboard cannot reach another host's daemon. Confirmed 2026-10-28.
- **O3 → the type scale wins** (stated, not objected to). 9.5 px uppercase tracked labels →
  `text-caption font-semibold text-muted`, sentence case. 32 px hero and 21 px stat
  numerals → `text-display`. 16 px page title → `text-title`. Diverges from the reference in
  size only; confirmed 2026-10-28.
- **O4 → one accent, gaps.** Diverges from the reference: four amber shades + a hatched
  reserve → every job segment `fill-accent`, 2 px gaps (MemoryBar's mask technique), reserve
  `fill-faint`, free `fill-surface-2`. Jobs are told apart by the legend and the segment's
  title, not by colour. No new colour roles. Confirmed 2026-10-28.
- **O5 → `Pipe` or two lines** wherever the reference draws `·`. Forced by lint.
- **O6 → new `SegmentedControl`** on Base UI's ToggleGroup, sharing Tabs' track classes.
  Confirmed 2026-10-28.
- **O7 → `StatusBadge`** kit part: soft role tint + role text, `rounded-xs`, as drawn.
- **O8 → visx 4.0.0** (`@visx/shape`, `@visx/scale`, pinned exactly, ADR-0045).

- **O9 → three more Base UI primitives** (owner 2026-10-28): `popover` for HostCard's peer
  list (read-only content; a `menu` promised actions it did not have), `meter` for
  BudgetRail (the rail gets `role="meter"` instead of being `aria-hidden`), and `collapsible`
  for DataTable's expandable rows (they had no control and no `aria-expanded`). Menu's
  `triggerContent` escape hatch, added only for HostCard, is removed with it.
- **O10 → DataTable's row toggle is a chevron in a lead column.** Diverges from the reference,
  which draws a row already open and no control to open it: a narrow first column holds a
  ghost ▸/▾ button on rows with detail, empty on rows without. Confirmed 2026-10-28.
- **O11 → The selected nav row keeps the accent wash** (`bg-accent-soft text-accent-text`), as
  drawn, not SettingsNav's quieter `bg-surface`: the main nav's "you are here" is louder than a
  sub-nav's. On it, a trailing count takes the row's `text-accent-text`, whatever its tone:
  `faint` (2.99:1 light, 2.53:1 dark) and `red` (4.08:1 dark) fail AA on the wash
  (`tests/design_tokens.rs`). The count's words stay in the link's name. Confirmed 2026-10-28.
- **O12 → Status words are sentence case** ("Delivered", "Timed out", "Live", "Stale"), per the
  guidelines, not lowercase as drawn. Confirmed 2026-10-28.
- **O13 → AttentionCard and AttentionRow stay two blocks.** They share the attention model
  (`ui/src/popover/attention.ts`), not markup: a full-bleed dense band in the popover, a card
  with body and actions on the page. Confirmed 2026-10-28.

## 3. Code archaeology

- `ui/dashboard/main.tsx` — the dashboard entry answers `/settings` with `Settings` and `/`
  with `Placeholder`. The new shell replaces `Placeholder` at `/` only; `/settings` keeps
  rendering `Settings` until it moves inside the shell.
- `ui/src/kit/ladder.ts` — kit parts take `size` and **no `className`/`style`**
  (`PassThrough`); a screen can never size or colour a control. New kit parts must follow.
- `ui/src/kit/Tabs.tsx` — the pill-track look the segmented control must match
  (`rounded-md bg-surface-2 p-3xs`, active `bg-surface text-fg-strong shadow-raised`).
- `ui/src/blocks/SettingsNav.tsx` — the existing sidebar look (`bg-surface-2`, selected row
  `bg-surface font-semibold text-fg-strong`, group label `text-caption font-semibold
  text-muted`) and its `narrow` collapse to a `Select`. The dashboard sidebar should read as
  the same family.
- `ui/src/blocks/MemoryBar.tsx` — the existing budget bar: SVG, 2 px gaps via a luminance
  mask (`design-tokens: allow` one-off), legend in GiB via `gib()`.
- `ui/src/blocks/PeerRow.tsx` — what a peer can show: name, runner count, last seen, stale
  after five minutes. Nothing more is known (O2).
- `ui/src/blocks/Pipe.tsx` — the only allowed separator between independent values (O5).
- `ui/src/theme/theme.css` — `--*: initial`; arbitrary values are a lint error outside the
  theme. A layout size (the sidebar width) is a named `--width-*` with its one use.
- `tests/design_tokens.rs`, `ui/scripts/theme.test.mjs` — enforce roles and scales; a new
  colour is a new role in both blocks of `desktop/src/tokens.css`.
- `npm run catalog:check` fails on any `@states` entry without a story.

## 4. Component inventory (step 1: components only)

Fidelity is `as-drawn` unless §2 records a confirmed divergence. 

| Element on artboard | Component | Status | Import | Fidelity | Artboards |
| --- | --- | --- | --- | --- | --- |
| Logo + "faber" + version | `Mark` | reuse | `ui/src/kit` | as-drawn | Overview, Nav |
| Sidebar with nav rows (icon, label, count) | `DashboardNav` | new (block) | `ui/src/blocks` | as-drawn; IA neutral → §2 O1 | Overview, Nav |
| Host card ("alpha, 2 hosts") | `HostCard` | new (block) | `ui/src/blocks` | diverges → §2 O2, O5 | Overview, Nav |
| Update card in sidebar foot | `UpdateCard` | new (block) | `ui/src/blocks` | as-drawn | Overview, Nav |
| Page header (title, live dot, actions) | `PageHeader` | new (block) | `ui/src/blocks` | diverges (size) → §2 O3 | Overview |
| Reconcile | `Button` secondary `lg` | reuse | `ui/src/kit` | as-drawn | Overview |
| Pause with ▾ (Drain) | `SplitButton` | new (kit) | `ui/src/kit` | as-drawn | Overview |
| Budget rail + legend | `BudgetRail` | new (block) | `ui/src/blocks` | diverges → §2 O3, O4, O5 | Overview |
| Stat tile (label, number, sparkline, note) | `StatTile` | new (block) | `ui/src/blocks` | diverges (size) → §2 O3 | Overview |
| Sparkline | `Sparkline` | new (kit chart) | `ui/src/kit` | as-drawn (visx, §2 O8) | Overview |
| Section heading + its card ("In flight", count line, action) | `Panel` | new (block) | `ui/src/blocks` | diverges (size) → §2 O3 | Overview |
| Segmented filter (All/Delivered/…) | `SegmentedControl` | new (kit) | `ui/src/kit` | as-drawn (§2 O6) | Overview |
| Data table (in flight, activity) | `DataTable` | new (block) | `ui/src/blocks` | diverges (header case) → §2 O3 | Overview |
| Peer list behind the host card | `Popover` | new (kit) | `ui/src/kit` | §2 O9 | Overview, Nav |
| Budget rail's accessible value | `Meter` | new (kit) | `ui/src/kit` | as-drawn (a11y only, §2 O9) | Overview |
| Expandable table row | `RowDisclosure` | new (kit) | `ui/src/kit` | diverges → §2 O10 | Overview |
| Result badge | `StatusBadge` | new (kit) | `ui/src/kit` | as-drawn (§2 O7) | Overview |
| Status dot | `StatusDot` | new (kit) | `ui/src/kit` | as-drawn | Overview, Nav |
| Needs-a-decision card | `AttentionCard` | new (block)* | `ui/src/blocks` | as-drawn | Overview |
| Host health rows | `KeyValueList` | new (block) | `ui/src/blocks` | as-drawn | Overview |
| Learned caps list | `KeyValueList` | new (block) | `ui/src/blocks` | as-drawn | Overview |

\* `AttentionRow` is the popover's full-bleed band, not a card; the dashboard card shares its
severity model and actions but not its layout (§2 O13).

## 5. Components

Every entry follows the kit's rules: no `className`/`style` props on kit parts (`PassThrough`
from `ladder.ts`), sizes from `size` (the ladder), colours from the ADR-0037 roles, spacing,
radius and type from the ADR-0038 steps, icons from `lucide-react`, a docstring with
`@useWhen` and `@states`, and one story per `@states` entry (`npm run catalog:check`). Copy
is sentence case, free text from the daemon is set as text, durations via
`fmtUptime`/`fmtAgo`. Data arrives as props; no component fetches.

Shared token map for the dashboard family:

| Drawn | Token |
| --- | --- |
| page bg / card / inset well | `bg-bg` / `bg-surface` + `border border-border` / `bg-surface-2` |
| card radius 11–14 px / row radius 8 px / badge 5 px | `rounded-lg` / `rounded-sm` / `rounded-xs` |
| card padding 12–15 px | `p-lg` (12) or `px-xl py-lg` (16 × 12) |
| 9.5 px uppercase label | `text-caption font-semibold text-muted` (sentence case) |
| 11–11.5 px secondary | `text-small text-muted` / `text-caption text-faint` |
| 12.5–13 px body | `text-body` |
| 16 px page title | `text-title font-semibold text-fg-strong` |
| 20–32 px numerals | `text-display font-semibold font-mono text-fg-strong` (tabular) |
| row hairline | `border-t border-border-soft` |
| green / amber / red + soft | `text-green` `bg-green-soft` etc. |
| accent / accent text / accent soft | `bg-accent`/`fill-accent`, `text-accent-text`, `bg-accent-soft` |

### Popover (kit, new — §2 O9)
- **For:** read-only content shown from a trigger: HostCard's list of other hosts. Not for
  actions (Menu) and not for a hint on hover (Tooltip).
- **Built on:** `@base-ui/react/popover` (Root, Trigger, Portal, Positioner, Popup, Title).
- **Props:** `label?` (the trigger's accessible name, only for a trigger with no visible text;
  a text trigger is named by its text, WCAG 2.5.3), `trigger: ReactNode` (what the trigger
  draws), `triggerVariant?: ButtonVariant` (default `ghost`; HostCard uses `row`), `size?`,
  `title?: string` (the popup's heading, e.g. "Other hosts"), `align?`, `children`.
- **States:** closed, hover, focus-visible, open.
- **Tokens:** trigger `buttonClass(size, triggerVariant, false)` + `w-full` for `row`; popup
  the same surface as Menu's (`rounded-md border border-border bg-elevate p-2xs
  shadow-floating`); title `text-caption font-semibold text-muted px-sm py-2xs`.
- **A11y:** Base UI's popover semantics (`aria-haspopup="dialog"`, focus moves into the popup,
  Escape closes and returns focus); `title` names the popup.

### Meter (kit, new — §2 O9)
- **For:** giving a drawn measure an accessible value: the budget rail ("12.0 of 18.0 GiB
  reserved"). It draws nothing itself.
- **Built on:** `@base-ui/react/meter` (Root only; the drawing is the caller's child).
- **Props:** `value`, `max`, `min?` (0), `label` (accessible name), `valueText` (what a screen
  reader says, e.g. "12.0 of 18.0 GiB reserved"), `children` (the drawn mark, `aria-hidden`).
- **States:** default, full, over max (value clamped for ARIA, `valueText` says the truth).

### RowDisclosure (kit, new — §2 O10)
- **For:** a table row that opens a detail row under it. The kit's `Disclosure` is a div row
  and cannot sit in a table.
- **Built on:** `@base-ui/react/collapsible` with `render` so Root is a `<tbody>` (one row
  group per row), the Trigger is a ghost square `md` button holding `ChevronRight` (rotates to
  down when open), and the Panel is a `<tr>` with one `<td colSpan>`.
- **Props:** `label` (accessible name of the toggle, e.g. "Details for acme/mobile build"),
  `open?`/`defaultOpen?`/`onOpenChange?`, `row: (toggle: ReactNode) => ReactNode` (renders the
  summary `<tr>`, placing `toggle` in its lead cell), `detail: ReactNode`, `colSpan`.
  `tone?: 'amber' | 'red'` carries a toned row's wash onto its detail row (a kit part takes
  no `className`, so the tone is a prop).
- **States:** closed, hover, focus-visible, open.
- **A11y:** the toggle carries `aria-expanded` and `aria-controls` (Base UI's).

### StatusDot (kit, new)
- **For:** a coloured dot that carries a state next to words that say it (never alone).
- **Props:** `tone: 'green' | 'amber' | 'red' | 'neutral' | 'accent'`; `pulse?: boolean`
  (running), `label?: string` (when the dot is the only carrier, becomes `role="img"` +
  `aria-label`; otherwise `aria-hidden`).
- **States:** each tone; pulse (honours `prefers-reduced-motion`: no pulse).
- **Tokens:** drawn as SVG (8 units, geometry inside a mark), `fill-green|amber|red|faint|accent`.
- **Tests:** stories per tone + pulse; render test that `aria-hidden` flips with `label`.

### StatusBadge (kit, new)
- **For:** a run's result or a class's status as a short label: `delivered`, `timed out`,
  `refused`, `infra error`, `retried`, `pinned`, `warned`.
- **Props:** `tone: 'green' | 'amber' | 'red' | 'neutral' | 'accent'`; `children` (text).
- **States:** each tone; long text truncates with ellipsis at its container.
- **Tokens:** `rounded-xs px-2xs py-3xs text-caption font-semibold`; tone → `bg-{tone}-soft
  text-{tone}` (neutral → `bg-surface-2 text-muted`, accent → `bg-accent-soft text-accent-text`).
  Contrast is the contrast test's job: the pairings must already pass for text.
- **A11y:** plain text; no role.

### SegmentedControl (kit, new)
- **For:** one choice among 2–6 peer filters over the same content (Activity: All /
  Delivered / Faults / Refused / Retried). Not Tabs: nothing here owns a panel.
- **Built on:** `@base-ui/react/toggle-group` (single value, required — never empty).
- **Props:** `items: { value: string; label: ReactNode; count?: number; tone?: 'amber' |
  'red' }[]`, `value`, `onValueChange`, `label` (accessible name), `size?: Size` (default `md`).
- **States:** default, hover, selected, focus-visible, with counts, toned item (Faults amber).
- **Tokens:** the track and active item reuse `Tabs`' exact classes (`rounded-md bg-surface-2
  p-3xs`; active `bg-surface text-fg-strong shadow-raised`; rest `text-muted
  hover:text-fg`), `ladder[size]`, `focusRing`. A count follows its label in `font-mono
  text-faint` (a toned item's label and count take the tone's text colour).
- **A11y:** Base UI's group semantics, arrow keys move, Enter/Space selects; the count is
  part of the item's accessible name ("Faults, 7").

### SplitButton (kit, new)
- **For:** a surface action with a less common sibling: Pause, and ▾ → Drain.
- **Props:** `variant` (Button's), `size` (default `lg`), `icon`, `children` (main label),
  `onClick`, `menuLabel` (accessible name of the ▾ trigger, e.g. "More pause options"),
  `items: { label, onSelect, tone?: 'danger' }[]`, `loading?` (spinner on the main half; the ▾
  half is disabled until it ends, so a contradicting sibling cannot start). `Menu` gains
  `disabled` for this.
- **States:** default, hover (each half separately), pressed, focus-visible (each half),
  menu open, loading.
- **Built from:** `Button` + `Menu` from the kit; one outlined box with a `border-border-control`
  divider between halves; the trigger is the square step of the same `size` with a
  `ChevronDown`.
- **A11y:** two tab stops; the trigger has `aria-haspopup="menu"` (Menu's) and `menuLabel`.

### Sparkline (kit chart, new — visx)
- **For:** the trend behind one number, inside a stat tile. No axes, no tooltip.
- **Props:** `values: number[]`, `tone?: 'neutral' | 'green' | 'amber' | 'red' | 'accent'`
  (default neutral), `label: string` (text summary, e.g. "Delivered rate over 24 hours,
  rising").
- **States:** data, one point (a dot), empty (renders nothing, keeps its box), flat line.
- **Built on:** `@visx/shape` `LinePath` + `@visx/scale` `scaleLinear`. Size `w-spark h-spark`.
- **Tokens:** stroke `stroke-{tone}` (neutral → `stroke-muted`), width 1.7 user units, round
  caps/joins.
- **A11y:** `role="img"` with `aria-label={label}`.

### Panel (block, new)
- **For:** a dashboard section: its heading line and the card that holds its content.
- **Props:** `title`, `meta?: ReactNode` (the quieter line after the title: "3 admitted,
  1 parked"), `action?: ReactNode` (right-aligned: a SegmentedControl, a count),
  `children`, `bare?: boolean` (heading only, children not carded — for a list of cards).
- **States:** default, with meta, with action, bare, empty (children say why — the panel
  never invents copy).
- **Tokens:** heading `text-body font-semibold text-fg-strong`, meta `text-small text-muted`,
  gap `sm` between heading and card; card `rounded-lg border border-border bg-surface`.
- **A11y:** `section` with `aria-labelledby` its heading (an `h2`).

### DataTable (block, new)
- **For:** a dense table: in-flight jobs, activity rows.
- **Props:** `columns: { key, header, width?: 'auto' | 'tight' | 'fill', align?: 'start' |
  'end', mono?: boolean }[]`, `rows: { key, cells: Record<string, ReactNode>, tone?: 'amber' |
  'red', detail?: ReactNode, detailLabel?: string, defaultOpen?: boolean }[]`, `caption`
  (accessible), `empty` (copy for no rows). When any row has `detail`, a lead toggle column
  is added (§2 O10); rows with detail render through `RowDisclosure`.
- **States:** default, toned row (amber/red soft wash), expanded row (detail under the row,
  spanning the columns), empty, long text (cells clip with ellipsis; expanded detail wraps).
- **Tokens:** `w-full`, auto layout (a fixed table cannot shrink a `tight` column to its content: `tight` cells are `w-px whitespace-nowrap`, the rest clip with `max-w-0 truncate`); header `text-caption font-semibold text-muted` sentence
  case, `px-lg pb-sm`; cells `px-lg py-md text-body`, `border-t border-border-soft`; `mono`
  cells `font-mono tabular-nums`; `align end` right-aligns; toned rows `bg-amber-soft` /
  `bg-red-soft`. Column widths are layout, not tokens: `tight` = shrink-to-content.
- **A11y:** a real `<table>` with `<caption class="sr-only">`, `<th scope="col">`.

### KeyValueList (block, new)
- **For:** a short list of name → value facts, optionally with a status dot: host health
  (Docker reachable, Disk 90 % used), learned caps (class → status → cap).
- **Props:** `items: { key, label: ReactNode, value: ReactNode, tone?: StatusDot tone,
  note?: ReactNode }[]`, `mono?: 'label' | 'value' | 'both'`.
- **States:** default, with dots, toned value (amber/red text), long label (truncates).
- **Tokens:** rows `flex items-baseline gap-sm py-2xs text-small`; label `text-muted`
  (or `font-mono text-fg` when mono), value right-aligned `font-mono text-fg` (toned:
  `text-amber`/`text-red` `font-semibold`).
- **A11y:** a `<dl>`.

### AttentionCard (block, new)
- **For:** one active issue on the dashboard that needs the operator: "Needs a decision —
  acme/mobile:build measured 6.4 GiB …" with its actions. Same severity model as the
  popover's `AttentionRow` (red, amber, neutral), laid out as a card.
- **Props:** `tone: 'red' | 'amber' | 'neutral'`, `title`, `body: ReactNode`, `actions?:
  { label, onClick, primary?: boolean }[]`, `pending?: string` (label of the action in
  flight), `error?: string`.
- **States:** amber, red, neutral, with actions, action pending, action failed.
- **Tokens:** card `rounded-lg bg-surface border` with the tone's border at the soft tint
  (`border-amber-soft`; neutral `border-border`); title `text-small font-semibold text-{tone}`
  with a lucide `TriangleAlert`/`CircleAlert` at the `sm` icon size; body `text-small text-fg`;
  actions `Button size="sm"`: one `primary` (the first marked `primary`, else the first), the
  rest `link` (guidelines § Button variants).
- **A11y:** `role="group"` named by its title; an error is `role="status"`.

### StatTile (block, new)
- **For:** one number that answers one question, with its trend and a one-line reading.
- **Props:** `label`, `value: ReactNode` (number + optional unit), `note?: ReactNode`,
  `tone?: 'neutral' | 'amber' | 'red'` (colours the value), `trend?: { values: number[];
  tone?; label: string }`.
- **States:** default, toned, with trend, no trend, loading (value shows "—").
- **Tokens:** `rounded-lg border border-border bg-surface px-xl py-lg`; label `text-caption
  font-semibold text-muted`; value `text-display font-semibold font-mono tabular-nums`
  (unit `text-small text-muted font-normal`); note `text-caption text-muted`; sparkline
  bottom-aligned with the value.

### BudgetRail (block, new)
- **For:** the memory budget, segmented by the job holding each slice — the Overview's hero.
- **Props:** `budgetMib`, `reserveMib`, `jobs: { key, label: string, mib: number }[]`
  (largest first), `admitted: number`, `parked: number`, `margin?: number`.
- **Layout (as drawn, sizes per O3):** a header line — `Memory budget` label left; right,
  "4.0 GiB free to admit" `Pipe` "margin 1.15". Then the figure: reserved GiB at
  `text-display font-mono`, "/ 18.0 GiB reserved" `Pipe` "67 %" quiet after it; right, two
  figures "3 admitted", "1 parked" (parked in `text-amber` when > 0). Then the rail. Then the
  legend: one entry per job (swatch, label, GiB), then free, then reserve.
- **Rail (per O4):** SVG `w-full h-rail`, segments in order jobs → free → reserve, 2 px gaps
  via a luminance mask as `MemoryBar` does, jobs `fill-accent`, free `fill-surface-2`,
  reserve `fill-faint`; each segment has a `<title>` ("acme/web e2e, 6.0 GiB").
- **States:** default, no jobs (rail is free + reserve; legend omits jobs), over budget (free
  absent, the reserved figure turns `text-amber`), many jobs (legend wraps; segments under
  1 % merge into "N more" in the legend and one segment).
- **Formatting:** GiB via `gib()` from `ui/src/popover/limits`.
- **A11y:** the SVG is `aria-hidden`; the legend is a list and carries the numbers.

### DashboardNav (block, new)
- **For:** the dashboard shell's sidebar: brand, host card, the nav list, an update card at
  the foot. IA-neutral (§2 O1).
- **Props:** `items: { id, label, icon: LucideIcon, href, count?: ReactNode, countTone?:
  'amber' | 'red' }[]`, `selected: string`, `mode: 'page' | 'section'`, `onSelect?(id)`,
  `version: string`, `host: ReactNode` (a `HostCard`), `footer?: ReactNode` (an
  `UpdateCard`).
- **States:** default, item hover, selected, focus-visible, with counts (toned count),
  with footer, no footer.
- **Tokens:** `w-sidebar shrink-0 bg-surface-2 border-r border-border p-lg flex flex-col
  gap-xl` (SettingsNav's family); brand row `Mark` + `text-title font-semibold` + version
  `font-mono text-caption text-faint`; rows `rounded-sm px-sm py-xs text-body gap-sm`, icon at
  the `md` icon size `text-muted`; selected `bg-accent-soft text-accent-text font-semibold`
  (as drawn), rest `text-fg hover:bg-border-soft`; count `ml-auto font-mono text-caption
  text-faint` (toned → `text-amber`/`text-red` `font-semibold`).
- **Responsive:** below `narrow` it becomes a `Select` of the items above the content, as
  `SettingsNav` does.
- **A11y:** `<nav aria-label="Dashboard">`; `aria-current` is `page` or `location` by `mode`;
  a count is in the link's accessible name ("Activity, 7 faults" — the screen passes
  `countLabel`).

### HostCard (block, new — diverges, §2 O2)
- **For:** which host this dashboard is, whether it is healthy, and the other hosts in its pool.
- **Props:** `name`, `tone: StatusDot tone`, `summary: string` (one phrase: "2 hosts, both
  healthy"), `peers: PeerRowModel[]` (from `ui/src/popover/peers`).
- **Built from:** `Popover` from the kit (§2 O9); the card is the popover trigger when `peers.length > 0`,
  otherwise a plain card (no chevron). The popup lists the peers as `PeerRow dense` (caption
  text, small padding; stale peers in `text-amber`), so the row is one block in both surfaces.
- **States:** default, no peers (no trigger, no chevron), with stale peer, popover open, hover,
  focus-visible.
- **Tokens:** `rounded-md bg-surface border border-border px-md py-sm`; name `text-body
  font-semibold text-fg-strong`; summary `text-caption text-muted`; trigger icon
  `ChevronsUpDown` → replaced by `ChevronDown` (it opens a list, it does not switch).
- **A11y:** the trigger is named by its visible text (host name and summary), with
  `aria-haspopup="dialog"`; the popup is a dialog titled "Other hosts", each peer a `group`
  named by its spoken label. Nothing in it is pressable.

### UpdateCard (block, new)
- **For:** a pending faber update, at the foot of the sidebar: "0.3.5 is ready — installs
  when this host goes idle", Install now / Skip.
- **Props:** `version`, `body`, `onInstall`, `onSkip`, `pending?: 'install' | 'skip'`,
  `error?: string`.
- **States:** default, installing (pending), failed.
- **Tokens:** `rounded-md bg-surface border border-border p-md`; title `text-small
  font-semibold text-accent-text`; body `text-caption text-muted`; `Button size="sm"`
  primary + `link`.

### PageHeader (block, new)
- **For:** a dashboard view's title row: title, a live indicator, the view's actions.
- **Props:** `title`, `live?: boolean | 'stale'` (dot + "Live" / amber "Stale", §2 O12), `actions?:
  ReactNode`.
- **States:** default, live, stale, with actions.
- **Tokens:** `flex items-center gap-md`; title `text-title font-semibold text-fg-strong`
  (`h1`); live `StatusDot` + `text-caption text-faint`; actions right, `gap-sm`.

## 6. Composition

- **Shell:** sidebar (fixed width, a named `--width-sidebar`) + main column. The sidebar is
  full height; its update card sits at the foot. Main column scrolls; the sidebar does not.
- **Overview main column:** page header → budget rail (full width) → four stat tiles in one
  row → two columns: tables (flexible) and the right rail (a named `--width-rail`).
- **Spacing rhythm:** section gaps `2xl`; card padding `lg`/`xl`; table cells `md`×`lg`.
- **Below 1280 px** (not drawn; to be confirmed with the screen step): the right rail drops
  under the tables first, then the stat tiles wrap to two per row; below `narrow` the sidebar
  becomes a `Select`, as `SettingsNav` does.

## 7. Out of scope (this step)

- The screens themselves: see [`screens.md`](screens.md).
- Data the daemon does not serve: time series for sparklines and "in the last hour", p95 wait,
  per-repo roster, latest drift per repo, the workflow scan. Components take these as props;
  where they come from is the screen step's problem.
- Moving Settings inside the shell, and retiring `INDEX_HTML` in `src/web.rs`.
- Light-mode references: the artboards are dark only; light is the theme's job and is checked
  in stories.
