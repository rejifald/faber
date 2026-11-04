---
title: faber design guidelines
type: spec
status: draft
date: 2026-10-18
owner: rejifald
---

# Design guidelines

These are faber's design rules. Most are faber's own decisions. The general rules near the
end hold for any user interface (§ General rules); a faber rule that departs from one says so
and gives the reason (§ Overrides of the general rules).

A design review reads this file on every UI PR, and each heading below becomes a row of the
review.

Each rule says **why** and how it is **enforced**: `test` or `review`. Its source is an ADR, a
design doc, or an owner decision with its date.

faber has two surfaces:

- **Tray popover:** React in `ui/` (ADR-0042): the screen `ui/src/screens/Popover.tsx`, its blocks
  in `ui/src/blocks/`, its data model in `ui/src/popover/`, and the entry `ui/popover/`, which
  the Tauri popover window loads.
- **Web dashboard:** React in `ui/`, a shell the daemon serves at each view (`/`, `/activity`,
  `/jobs`, `/memory`, `/maintenance`, `/settings`, ADR-0054, ADR-0056). The app's panel window (⌘⇧F) is this dashboard on a
  one-time link, with no IPC (ADR-0041, ADR-0028 addendum C).

## Sources

- **React with Base UI and Tailwind** ([ADR-0042](../adrs/0042-the-ui-is-react-with-base-ui-and-tailwind.md),
  owner 2026-10-18). The popover and the dashboard are one app in `ui/`. Base UI parts are
  wrapped once in `ui/src/kit/`, and screens use only the kit. Tailwind's `@theme` holds every
  token, with no arbitrary values outside `ui/src/theme/`. Icons come from `lucide-react`.
  What each kit part is for, its states and props are in the generated
  [catalogue](catalog.md). *Enforced:* lint (`ui/eslint.config.mjs`: raw colours, arbitrary
  values, ladder sizes outside the kit, Base UI imports and raw controls outside `ui/src/kit/`,
  `style` outside the kit), tests (`npm test` in `ui/`) and `npm run catalog:check`, all in
  CI's `ui` job. The popover and the dashboard have moved; the panel window shows the
  dashboard, and the vanilla panel and inline page are gone.
- **One colour source for every surface** ([ADR-0037](../adrs/0037-every-surface-colours-from-one-token-source.md),
  owner 2026-10-18): `desktop/src/tokens.css`, with semantic role names and a light block on
  `:root` and a dark block under `prefers-color-scheme: dark`. The React theme imports
  it; the daemon embeds it into the sign-in and standby pages. Fonts are `--sans`
  and `--mono`. *Enforced:* test (`tests/design_tokens.rs`).

- **Charts are kit blocks on visx** ([ADR-0045](../adrs/0045-charts-are-kit-blocks-on-visx.md),
  owner 2026-10-19). Screens use chart blocks, never visx directly; charts are styled only
  through the theme and hold the contrast tiers. Tremor is a visual reference, not a
  dependency. *Enforced:* lint (the kit-import rule), once built.

## Layouts

- **The popover is a card of one of two widths**, drawn in light and dark (owner, 2026-10-26,
  #368; was: a fixed 308 px card). While job groups show it is **620 px in two columns**: the
  gauges and other hosts on the left, the job groups on the right. Otherwise (idle, offline,
  loading, the Limits view) it is **308 px in one column**. Each width is a theme size
  (`w-popover-wide`, `w-popover`). The transparent window is the card plus 36 px a side for the
  shadow, and follows the card's width with its right edge fixed, so the beak stays under the
  menu-bar icon (`resize_popover` in `desktop/src-tauri/src/lib.rs`). There are no breakpoints
  beyond the two widths. *Enforced:* review.
- **The popover's content and its order are frozen** by [`popover.md`](popover.md): attention,
  then the gauges, then jobs, then one quiet line each for everything else. In two columns the
  gauges lead the left column and jobs fill the right one, so the order still reads left to
  right. Change `popover.md` before changing what the popover shows. Its look is specified by
  [`popover/anatomy.md`](popover/anatomy.md). *Source:* owner, 2026-10-17 and 2026-10-26.
  *Enforced:* review.
- **Jobs never make the popover taller.** In two columns the job groups fold at the gauge
  column's height: the first groups in full, then one compact line per further group, then
  "Show N more", which opens the rest in place without making the column taller
  (popover/fold.md). *Source:* owner, 2026-10-25, 2026-10-26 (#368) and 2026-10-31.
  *Enforced:* review.
- **A healthy popover is calm and short.** The default state adds nothing: no status chip, no
  idle text line, and no uptime. *Source:* `popover.md`, owner 2026-10-17. *Enforced:* review.
  One exception: while a dismissed row's cause is still active, its "N dismissed" line stays,
  even with every other row gone ([ADR-0064](../adrs/0064-a-warning-that-does-not-stop-jobs-can-be-dismissed.md)
  § 2: nothing disappears silently). *Source:* owner, 2026-11-01 (#483 design review).

## Navigation

- **The dashboard is the one full UI** ([ADR-0041](../adrs/0041-the-dashboard-is-the-one-full-ui.md),
  owner 2026-10-18). The popover is the glance surface, and the panel window shows the
  dashboard. No section exists in two places.
- **The dashboard is views in a sidebar** ([ADR-0054](../adrs/0054-the-dashboard-is-views-in-a-sidebar.md),
  owner 2026-10-28, superseding ADR-0041's one page in four groups): Overview, Activity,
  Jobs ([ADR-0056](../adrs/0056-a-run-records-where-its-time-and-resources-went.md)), Memory,
  Maintenance and Settings, each a flat URL; Repositories joins with its own step.
  Every action that changes what the host does lives on Maintenance, beside the state it
  changes; an alarm card offers only the one fix it names
  ([screens.md S13](dashboard/screens.md)). Overview holds
  every alarm and a summary of each other view, linking to it; a section's full form lives in
  one view only. A new section joins a view; a new view changes the ADR. *Enforced:* review,
  and the front door's one list of view paths (`src/control.rs`).

- **Left-click on the menu-bar icon opens the popover.** It closes on blur. Its **⋯** menu
  holds the panel, the dashboard, Start at login, and Quit. *Source:* `popover.md`.
- **The ⋯ menu is HTML drawn as a macOS menu, not a native `NSMenu`,** because a native menu
  takes key focus and the popover closes on blur. Escape or a click outside closes it.
  ⌘⇧F and ⌘Q work with the menu closed. *Source:* `popover.md`, owner 2026-10-17.
  *Enforced:* review.

## Colour

- **A colour is a token.** A new colour is a new role in *both* theme blocks of
  `desktop/src/tokens.css`, never a literal anywhere else, and never a second token block in
  another stylesheet. *Why:* a literal does not follow the dark theme and drifts from the
  palette. *Source:* ADR-0037. *Enforced:* test (`tests/design_tokens.rs`,
  over `desktop/src/*.css` and every `<style>` block the Rust sources under `src/` embed; it
  also checks that both theme blocks define the same roles; `ui/scripts/theme.test.mjs`
  covers the React theme's import of it).
- **Every surface follows the system theme,** the web dashboard included. It gets the faber
  palette in light and dark, with links and its primary on the amber accent. A value only
  one surface needs, such as the popover's floating shadow, is its own role in the shared
  file. *Source:* ADR-0037, owner 2026-10-18. *Enforced:* review.
- **Contrast meets WCAG AA:** 4.5:1 for text, 3:1 for large text and UI marks (focus rings,
  tab underlines, checkboxes, status dots, note markers), in both themes and on every page
  surface. Accent-coloured text, focus rings, tab underlines and checkboxes use
  `--accent-text`. `--accent` is held to 3:1 as a mark and fill (the gauge arc, the budget
  bar, the primary behind `--accent-ink`, the logo); it is too light for text.
  *Why:* the dashboard's light mode put amber links on the page at 2.69:1. *Source:* owner,
  2026-10-18. *Enforced:* test (`tests/design_tokens.rs`, over the token pairs in
  `tokens.css`).
- **Control boundaries hold 3:1** (WCAG 1.4.11): the edge of every kit control (Checkbox,
  Input and Field, the Select trigger, the outlined `secondary` button) is drawn in
  `--border-control`, at 3:1 or more on `bg`, `surface` and `surface-2` in both themes.
  `--border` and `--border-soft` stay for dividers and containers, which are not controls.
  The vanilla surfaces do not use the role yet. *Source:* owner, 2026-10-18 (#280 design
  review). *Enforced:* test (`tests/design_tokens.rs` for the values); review for which
  boundary uses it.
- **Override: `--faint` text is held to 3:1, not 4.5:1.** WCAG has no 3:1 tier for
  normal-size text, so this is a product decision, not WCAG's incidental-text exemption. It
  keeps faint hints and timestamps a step quieter than `--muted`. *Source:* owner,
  2026-10-18. *Enforced:* test.
- **A genuine one-off** (a mask stop, a fully transparent fade stop) carries
  `/* design-tokens: allow (<reason>) */` on the same line. The test rejects a marker without
  a reason.
- **The binding ring takes the accent, or amber while something waits.** Attention rows are
  coloured by severity: red, then amber, then neutral. *Source:* `popover.md`. *Enforced:*
  review.

## Scales

- **Spacing, radius and type come from named scales** in the Tailwind theme (`ui/src/theme/theme.css`)
  ([ADR-0038](../adrs/0038-spacing-radius-and-type-come-from-named-scales.md), owner
  2026-10-18). Spacing has ten steps (2, 4, 6, 8, 10, 12, 16, 20, 24, 32). Radius is
  `xs` 4 (small marks: checkboxes, tiny badges; added 2026-10-18), `sm` 6, `md` 10, `lg` 14
  and `full`. Each step has a use line in the ADR. A value is
  snapped by its role, not by the nearest number. A missing step is a change to the scale,
  never a local value. *Enforced:* in `ui/`, the theme clears Tailwind's defaults so only these
  steps exist, lint rejects arbitrary values, and `ui/scripts/theme.test.mjs` holds the theme
  to this ADR's tables. The vanilla surfaces are not on the scale yet.
- **A layout size is named in the theme, with its one use,** since a class cannot carry a raw
  value: `--width-popover` (the 308 px card), `--width-popover-wide` (the 620 px two-column
  card), `--width-gauge-col` (its 240 px gauge column), `--height-gauge` (a gauge bar's 6 px
  track), `--width-age` (the popover's 30 px age column), and the dashboard's `--width-sidebar`,
  `--width-rail`, `--height-rail`, `--width-spark` and `--height-spark`. The full list, each with
  its one use, is `ui/src/theme/theme.css`. ADR-0038 § 6 leaves widths and
  heights raw; naming them keeps them out of the spacing scale (`w-popover` exists,
  `p-popover` does not). Geometry inside a drawn mark (the gauge bar's radius and fit mark,
  the 9 px pip glyphs and the 56 × 6 overflow bar) is SVG units in that block. *Source:* the
  popover build, 2026-10-18. *Enforced:* lint (no arbitrary values) and review.

## Typography

- **Bundled Inter for `--sans` and IBM Plex Mono for `--mono`** (ADR-0040, owner
  2026-10-18), the same on every OS. Mono is for code, commands, versions, IDs and values
  that align; the dashboard's body text is the sans. No other face is named in a stack.
- **Five sizes, 11 px floor:** `caption` 11, `small` 12, `body` 13, `title` 15, `display` 20.
  No half pixels. *Why:* text below 11 px is too small to read comfortably, and 16 sizes
  had no system to them. *Source:* ADR-0038, owner 2026-10-18. *Enforced:* in `ui/`, the
  theme has only these five `text-*` steps and `ui/scripts/theme.test.mjs` checks them
  against ADR-0038. The vanilla surfaces are not on the scale yet.

## Copy

- **English, sentence case.** *Source:* owner, 2026-10-17. *Enforced:* review.
- **Durations go through `fmtUptime` / `fmtAgo`** (`ui/src/popover/format.ts`), never formatted
  by hand.
  *Enforced:* review.
- **Free text from the daemon or GitHub is inserted as text nodes, never as HTML.** For refused
  fork and untrusted-actor jobs, the attention row shows only the repo, because the outsider
  controls the workflow and job names. *Why:* that text is attacker-controlled. *Source:*
  `popover.md`. *Enforced:* review.
- **No dots or bullets between facts in the React UI** (owner 2026-10-18). Copy in `ui/` follows
  the separator rule: one phrase, or two lines, and a spaced pipe in quieter ink,
  hidden from screen readers, only between values that really stand apart (the `Pipe` block).
  *Enforced:* lint (`design-tokens/no-dot-separator` rejects `·` and `•` in strings, templates
  and JSX text anywhere in `ui/src`).
- **The vanilla screens that kept their `·`,** the panel's `main.js` and the daemon's
  `INDEX_HTML`, are retired (2026-10-28); their React replacements follow the rule above.

## Controls

- **Pause/Resume is the popover's one button.** Reconcile is a small secondary control,
  because the daemon reconciles on its own. *Source:* `popover.md`. *Enforced:* review.
- **Every control sizes from one ladder, the kit's `size` prop** ([ADR-0039](../adrs/0039-every-control-sizes-from-one-ladder.md), ADR-0042,
  owner 2026-10-18). `sm` is 20 px, for inline row actions and chips. `md` is 26 px, for menu
  items, the ⋯ button, tabs and inputs. `lg` is 32 px, for a surface's own actions. Each step
  sets height, padding, label, icon, gap and radius together, and a square variant serves
  icon-only buttons. The height is a minimum and the icon (and a checkbox's box) scales with
  the label, so enlarged text grows a control instead of crowding it; at 100 % nothing changes
  (ADR-0039 § 1, amended 2026-10-18). A control never sets its own size, and a missing size is a change to the
  ladder. *Enforced:* `ui/scripts/theme.test.mjs` holds `ui/src/kit/ladder.ts` to ADR-0039's
  table, the kit parts take no `className` or `style`, and lint rejects `control-*` and
  `icon-*` sizes outside the kit and as spacing inside it.
- **A checkbox is boxed on the ladder:** it takes `size` (default `md`), with the step's
  label and gap and a box the size of the step's icon, and no height of its own. *Source:*
  ADR-0039 § 2 amendment, owner 2026-10-18. *Enforced:* test (`ui/scripts/theme.test.mjs`,
  `ui/scripts/kit.test.mjs`).
- **Disabled only while a sibling action runs** (owner 2026-10-18; it settles, for faber, whether
  disabled is ever right). A control is disabled only while another
  action on the same thing is in flight: Reset while Save settings runs. Navigation is never
  such a sibling: tabs stay live while a save runs, since the save finishes either way and its
  result shows on return, so Tabs has no disabled state (owner 2026-10-19). Any other "can't do
  this now" stays live, and pressing it says why: a note inline beside the pressed control
  (`role="status"`), which clears on the next interaction or when the condition changes. No
  toast. Its tooltip may preview the reason. *Enforced:* review; the kit's `disabled` props say
  so, and the Button stories `Unavailable`, `UnavailablePressed` and `DisabledWhileSiblingRuns`
  show both.
- **Button variants:** `primary` for a surface's one main action; `link` (link-tone text, no
  fill or border, the step's full tap height) for secondary and rarely used actions; the
  outlined `secondary` for neutral tool buttons only; `ghost` inside dense rows; `row` for a
  whole row that is one button, such as a settings profile row, whose hover fill (the border
  role) must show on the background it sits on (owner 2026-10-22, #342 design review);
  `danger` for a confirmed destructive action, such as Remove in a confirmation row (owner
  2026-10-22); `tone="danger"` on a secondary for an action that starts a destructive flow,
  such as Remove before its confirmation. An empty state's call to action is `primary` only
  while nothing else is: when the save bar's Save shows, it drops to the outlined `secondary`.
  Settings applies the rule as written, with no exception: Discard, Cancel and Change backend
  are `link`; Retry, Reload, Add, Add profile, Copy command, a trust row's Undo and a field's
  narrowing button (such as Clear) are tool buttons. The per-item Undo on a marked list or map
  row stays `link`, small, beside the value it restores.
  A trust-change attention row's Acknowledge is a tool button too, like the row's other inline
  actions ([ADR-0064](../adrs/0064-a-warning-that-does-not-stop-jobs-can-be-dismissed.md), 2026-11-01).
  *Source:* owner 2026-10-18 (#280 design review), owner 2026-10-24 (#343 design
  review). *Enforced:* review; the catalogue states
  it in Button's use line.
- **A list line that is pressed is a `RowButton`, off the ladder.** The last line of a folded
  popover list ("Show N more", "Show fewer", "Open panel") is a line of that list, not a
  control beside it: it takes the list's own grid (`px-xl`, `py-3xs`, `text-small`), full
  bleed, no fill or radius at rest, and the row-hover and row-press tints, so it sizes like the
  compact lines it ends rather than from a ladder step. Button's `row` variant stays for a
  whole row that is one button and sits on the ladder (a settings profile row). *Source:*
  owner 2026-10-31 (popover/fold.md, #450 design review). *Enforced:* review; the catalogue
  states it in RowButton's use line.

## Icons

- **Icons are Lucide, through `lucide-react`** ([ADR-0040](../adrs/0040-icons-are-vendored-lucide-fonts-are-bundled-inter-and-plex-mono.md),
  amended by ADR-0042; owner 2026-10-18). faber's logo and offline glyph are kit
  components. An icon Lucide does not have is drawn in its grid and 2 px stroke. Sizes come from the control
  ladder (12, 14, 16). There are no Unicode symbols standing in for icons. *Enforced:*
  review.
- **The popover's pips are drawn marks, not icons** (owner, 2026-10-25, #369). They are
  9 px glyphs in SVG, below the icon ladder and off Lucide's grid, because six of them and the
  job's age must fit in a 308 px row beside the PR name. They are decorative (`aria-hidden`), and
  the row's accessible name says the same in words. New pip shapes are added in
  `ui/src/blocks/PipStrip.tsx` and its anatomy (`popover/anatomy.md` § 5), never with Lucide
  icons scaled down. *Enforced:* review.

## States

- **The attention list shows every active issue as its own row,** sorted by severity, at most
  three, with "Show N more" opening the rest in place (popover/fold.md, 2026-10-31). One banner slot hid all but the first issue, which is
  why the popover was redesigned. *Source:* `popover.md`. *Enforced:* review.
  Since [ADR-0064](../adrs/0064-a-warning-that-does-not-stop-jobs-can-be-dismissed.md)
  (2026-11-01) the operator may take a row out of it: a row that does not stop jobs carries
  Dismiss and, dismissed, folds into one "N dismissed" line under the list (and out of its
  tally), which opens to Restore it. A blocking row (`blocking: true`) always shows as its own
  row. Acknowledged trust changes fold into one neutral row that opens to list them. The
  dashboard's alarms and Memory's warning cards follow the same rule. *Enforced:* review; every
  new row is classified by "does this stop jobs?".
- **Every state has a story.** The React kit's stories run in Storybook (`npm run storybook`
  in `ui/`, [ADR-0043](../adrs/0043-stories-run-in-storybook.md)), with Theme and Text size
  switches in the toolbar. Each state in a part's `@states` docstring has its own story, and
  `npm run catalog:check` fails on an entry without one. *Enforced:* catalogue check and review.
  The popover's screen has a story per state. The frozen references in
  [`popover/reference/`](popover/reference/) hold for its rows and marks; since the two-column
  card (owner, 2026-10-26, #368) the stories win for its layout, since the frozen references draw the
  308 px card with rings. There is no visual regression yet.
- **A forced state lands on one control** (owner 2026-10-18): the one a pointer or the keyboard
  would be on, never a whole row. A hover story forces one unselected tab, and a highlighted-item
  story highlights one menu or select item. A part whose variants draw hover or pressed
  differently gets a story per variant, each beside a resting one. *Enforced:* review; a story
  names its element in `parameters.force`, which marks only the first match.

## General rules

These come from the owner's rules for any user interface rather than from a faber decision.
They are stated here so that this file is complete.

- **No bare default states.** Show only the unusual state, and phrase it; the default state
  shows nothing extra. Don't label the normal case with a bare word ("in use", "active").
  *Why:* a word on every row is noise; the one that differs is what the user needs to see.
  *Source:* owner, 2026-10-16. *Enforced:* review.
- **User data exactly as entered.** Names, titles and notes render in the user's own spelling
  and case. Don't change their case, inflect them or rephrase them to fit a sentence; rewrite
  the sentence instead. *Why:* changing a user's words tells them the product knows better.
  *Source:* owner, 2026-10-16. *Enforced:* review.
- **One filled primary per place.** A place (a screen, a sheet, an opened row) has at most one
  action filled with the accent. Secondary and rarely used actions are link-tone text buttons
  at the full tap height of their step. *Why:* saturated colour is spent once per moment, so
  the next step is never in doubt. *Source:* owner, 2026-10-16. *Enforced:* review.
- **Rows that ask for action open as a whole.** A row that asks the user to act is pressable
  as a whole: collapsed, it shows its content and a chevron and no buttons; opened, it shows
  its actions, with one filled primary. A row that shows a fact may keep one small inline
  action. Every action is reachable by a press, never only by a swipe. *Why:* buttons on every
  row repeat one choice per row and hide the content; opening the row is the choice.
  *Source:* owner, 2026-10-16. *Enforced:* review. faber's attention rows override it (below).
- **Show the options on real screens before asking.** A visual question (layout, navigation,
  colour, borders, spacing) goes to the owner with each option rendered on the real screens,
  side by side, never in text only. *Why:* an option that reads well in words can look wrong
  when it is drawn. *Source:* owner, 2026-10-16. *Enforced:* review: a visual judgement is
  listed as needing the owner's decision, never guessed.

## Overrides of the general rules

- *Retired 2026-10-28:* the vanilla screens' exemption from "no dots or bullets between
  facts". The panel and the inline dashboard it covered are deleted (ADR-0054), so the rule
  now holds everywhere.
- **Attention rows carry their own inline buttons** (owner 2026-10-28, with the Pin button, #414). A popover
  attention row is a full-bleed band, not a card, and its fix is one press away, so its actions
  (Release, Clear, Pin, Acknowledge) sit in the row on the `sm` step instead of in a surface's
  action row. Dismiss is a quiet `sm` ghost × at the row's end, never in its action row
  ([ADR-0064](../adrs/0064-a-warning-that-does-not-stop-jobs-can-be-dismissed.md), 2026-11-01).
  An action that grants more than faber would on its own asks in the row first (`confirm`):
  Cancel, then the action, focus on Cancel.

## Considered and not adopted

- **A `cargo test` convention test as the token gate** (2026-10-18): it was the right gate
  for vanilla CSS, and it stays for `desktop/src` until that code is retired. The React app
  is gated by ESLint (ADR-0042 § 8).
- **Finishing the vanilla-CSS builds of ADR-0038 to ADR-0040 first:** stopped in favour of
  building them into the React kit (ADR-0042).
