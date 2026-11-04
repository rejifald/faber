# Tray popover, menu-native restyle with job pips: anatomy

Reference: [`./reference/`](reference/) (frozen 2026-10-17; each artboard in `-dark` and
`-light`) · Decided: 2026-10-17 by the owner · Content canon:
[`../popover.md`](../popover.md)

Re-rendered on 2026-11-03 from the Storybook stories, with illustrative data; the frozen originals
stay with the legacy repository.

**Built** in React in `ui/` (ADR-0042), as `ui/src/screens/Popover.tsx` and its blocks, with a
story per state, every pip included. The done pip reads `outcomes[].conclusion`,
which the daemon change in § 5 added under
[ADR-0044](../../adrs/0044-a-finished-run-shows-ci-s-verdict-only-as-github-reported-it.md).

**Amended 2026-11-01 (owner): a job's own CI failure is not faber's to report.** Only failures
faber caused matter here. A job whose tests caught a bug, or whose code is broken, finished on a
runner faber delivered, so it draws the same muted **done** check as a job that passed, for the
same 30 s. The red CI-failed cross, its 10 min linger, its taller bar segment and its red
"web unit failed" line are gone; red now means faber's fault and nothing else. GitHub's verdict
stays in the job line's hover title. Where this file still describes the passed and CI-failed
pips below, the amendment wins; the reference PNGs predate it.

§ 3 and § 4 name the vanilla code that build replaced; the React parts are listed in
[`../catalog.md`](../catalog.md).

**Where the build departs from the reference images** (owner, 2026-10-19). The images predate
these rules, so a review should not flag the build for them:

- the "Other hosts" label is sentence case, with no uppercase transform (the images draw it
  uppercase; guidelines § Copy);
- attention-row actions are on the `sm` step, as ADR-0039 § 2 says (the images draw them larger);
- a stale peer's pips are amber rings, per the token list in § 5 (the images draw them grey);
- live groups sort by oldest job first, groups running here before groups that only wait, per
  § 6 (`busy-overflow` draws `demo/PR-2750` first);
- a healthy idle popover has no Reconcile button, per `popover.md` § Healthy idle (the `idle-*`
  images draw one);
- since the third pass (owner, 2026-10-26, #368), the card is two columns at 620 px while jobs
  show, the gauges are bars rather than rings, a peer is a runner count with no pips, and
  folded groups are compact lines (§ 2 *Third pass*, § 5, § 6). The frozen reference images draw
  the 308 px card with rings: they still hold for the header, the attention band, the group rows,
  the pips and the actions row, and the `Screens/Popover` stories win for the layout. The ten
  screen references re-rendered on 2026-11-03 (`idle-*`, `busy-*`, `busy-overflow-*`,
  `paused-frozen-*`, `idle-peer-stale-*`) are those stories.

**Decided with the verdict pips** (owner, 2026-10-19, #287). § 2 and § 5 left these open. The
build proposed an answer for each, and the owner confirmed all three:

- a run whose runner faber lost draws the fault ring even when CI then concluded `failure`, since
  the ring says whose fault the red verdict is (`JobGroup › FaultOnly` beside `CiFailedOnly`);
- a finished-only group whose CI failed in some jobs and whose runner failed in others joins both
  in one phrase: "web unit failed, runner failed for lint";
- a passed job's line in an open group says "passed" in `--faint`, as a step count does, since § 5
  colours only the red words (`JobGroup › PassedOpen`).

This is the build spec for the popover's second pass in one day. `popover.md` decided
**what** the popover shows. This file decides **how it looks**, and adds one piece of
content: pips for jobs that just finished. A builder reads only this file and the
reference images. Anything this file does not state is not licensed.

## 1. The screen

- **Purpose:** answer "does faber need me?" at a glance (`popover.md`).
- **Who:** the operator, who clicks the menu-bar icon. A healthy popover is calm and short.
- **Data:** the daemon's `GET /api/state` body, passed verbatim to the webview (#180):
  - `inflight[]`: jobs running or waiting;
  - `outcomes[]`: recently finished runs, extended by this change;
  - `peers[]`, `budget` (gauges), plus the attention sources already listed in
    `popover.md`.
- **Entry point:** a left click on the tray icon (`desktop/src-tauri/src/lib.rs`). The
  popover closes on blur.
- **Viewport:** a 620 px two-column card while any job group shows, otherwise a 308 px single
  column; light and dark (owner, 2026-10-26; was: a fixed 308 px card). The window follows the
  card (§ 6). There are no other breakpoints.
- **Sparsest data:** online, healthy, nothing in flight, no peer
  ([`idle-*.png`](reference/)). The densest is
  [`busy-overflow-*.png`](reference/): one group over the pip limit plus three more groups.

## 2. Decisions

All of these are by the owner on 2026-10-17, chosen from rendered options, unless dated
otherwise. The rejected options are listed so review does not reopen them.

**Third pass: two columns and bars** (owner, 2026-10-25 and 2026-10-26, #368, from the
since-removed `Prototypes/Popover two-column` stories; `Screens/Popover` Busy, Folded and
BusyBeyondCap now show the result). These supersede the ring and peer-pip decisions
below where they disagree.
- **Two columns while jobs show.** A 620 px card: the header, the attention band and the
  actions row span it; between them, a 240 px gauge column (this host's gauges, other hosts,
  the VMs line) and a jobs column. With no job group the card is the 308 px single column.
- **Jobs never make the card taller.** The jobs column folds at the gauge column's height:
  full group rows while they fit, then a **compact line** per further group, then
  "Show N more" (the `FoldToggle`; since 2026-10-31 it opens in place, see [`fold.md`](fold.md)).
- **Bars everywhere.** Each gate is a `GaugeBar`: caption and real figures over a 6 px bar.
  The idle 308 px card stacks the same bars. The ring block retires.
- **A gate opens in place** (owner, 2026-10-25, gauges direction A; built 2026-10-26, #370):
  each `GaugeBar` is the summary of a `GateRow` disclosure, like a job group, and its open
  content lists what the bar is made of (`popover.md` § Hero), then **Adjust limits** into the
  Limits view. One gate open at a time. This replaces the gauges as one "Adjust limits" button
  (2026-10-22); the ⋯ menu's *Limits…* stays.
- **A peer is a count.** `peer c3d4` · "2 runners" · age, with no bar or pips: the snapshot
  has no peer ceiling (ADR-0004), so any mark would read full.
- **A compact line's mark is one pip**, the group's worst state, in #369's glyphs, so a folded
  group reads like a full row. Its accessible name is the full row's.
- *Rejected* (2026-10-25): the folded groups as only the link row, as a count in a column
  header, or as one summary row with every pip merged.
- *Rejected* (2026-10-26): a compact line marked by a filled dot before the name (green meant
  running there and passed elsewhere), every pip, or a pip only for trouble.
- *Rejected* (2026-10-26): a peer as a segmented "N / N" bar, which read saturated with no
  ceiling known.

**Layout and style ("menu-native")**
- There are no cards inside the card. The ring row and job groups lose their own
  border and fill; sections are separated by spacing and one hairline (`--border`).
  The look is a macOS menu, not a web dashboard.
- Rings shrink to 34 px and sit four across with the value under each ring. The fit
  marks, the binding colouring and the "binds" caption are unchanged from `popover.md`.
- Hovering a group row fills it edge to edge with the accent, like a macOS menu
  selection. Text and pips on that row switch to the ink colour.
- The actions row is the last row, above a hairline. **Pause and Reconcile are ghost
  buttons**: no fill or border at rest, and the `--surface-2` fill on hover or press.
  **While paused, Resume is the filled accent button** (`.po-btn.primary`, as today).
- *Rejected:* one large ring for the binding gate. It brought back hero emphasis the
  owner had removed, and the layout jumped whenever the binding gate changed.
- *Rejected:* a mono "instrument" tone with uppercase column headers. Too busy.

**Job group rows**
- A row has three parts: **name · pips · age**. The status dot, the branch, the chevron
  and the `● n ○ n` glyph counts are removed.
- An open group gets a `--surface-2` fill instead of a chevron. Its jobs are listed
  under it as **job name · step `i/n` · elapsed**. The live step's *name* moves to the
  job line's `title` (hover). One group open at a time, as today.
- A group where every job waits shows the reason under the row in amber
  ("waits for memory"), as today.
- *Rejected:* "2 running · 1 waiting" in words. It made rows wider and truncated names
  earlier.
- *Rejected:* two-line rows. They made every row about 60% taller.

**Pips: one per job**

| Pip | Means | Data |
|---|---|---|
| green arc turning on a faint ring | running (`admitted`, not frozen) | `inflight[]` |
| amber pause bars, still | frozen by the disk floor (ADR-0032) | `inflight[].frozen` |
| amber clock ring, still | waiting (`admitted: false`) | `inflight[]` |
| muted check, fades out | **done**: CI concluded `success`, `failure` or `timed_out`, whichever; shown for 30 s after the verdict arrives (owner, 2026-11-01; was: a green **passed** check for `success` and a filled red **CI failed** cross for 10 min) | `outcomes[].conclusion`, `outcomes[].conclusion_ts_ms` |
| red ring with a slash | **faber's fault**: `result` is `failure`, `infra_error` or `timeout`; shown 10 min | `outcomes[].result` (exists) |
| none | `cancelled`, `skipped`, `neutral`, `reclaimed`, or a conclusion faber could not read | — |

- Pips are ordered: finished (done, fault), then running, frozen, waiting.
- **The shape carries the state** (owner, 2026-10-25): each pip reads without its colour,
  for colour-blind readers. Frozen is
  amber, like its "frozen for disk" note (was: green at 40%).
- **More than 6 pips in a group** (live plus lingering) draw as the job count and a
  56 × 6 bar with one segment per job, in strip order and the pips' colours (owner,
  2026-10-25; was: `running/total`, which hid how many failed or wait). 6 matches the
  slot ceiling.
- **A group whose only remaining entries are finished keeps its row** while any pip
  lingers. The age counts from the newest outcome. A red line under the row names a
  faber fault ("runner failed for web unit"; "2 runners failed"). A CI failure gets no line
  (owner, 2026-11-01).
- A faber fault and a done pip can both appear for the same job, because the
  auto-retry of ADR-0003 re-runs a faber-caused failure. Each attempt is its own pip.
- **Cancelled shows no pip.** A cancelled run is usually replaced by a newer push, and
  it is not a verdict on the code.
- *Linger:* done 30 s, with the fade over the last 10 s. Red ring 10 min.
- *Linger start* (owner, 2026-10-19, #287): the done pip counts from when the
  daemon read the verdict (`conclusion_ts_ms`). The daemon reads it about 5 s after the run ends
  (ADR-0044 § 3, amended), so a verdict that arrives late still shows for its full time. With an
  older daemon that sends no `conclusion_ts_ms`, they count from the run's end (`ts_ms`). The red
  ring has no verdict, so it still counts from `ts_ms`.

**Peer host** (superseded on 2026-10-26 by *Third pass*: a peer is `peer <instance>` · "N
runners" · age, amber when stale, with no pips or bar; the rest of this block is history)
- The peer is a **row like a job group**: `peer <instance>` · one neutral grey rounded
  square per runner (`--faint`; the square says "not your job") · age since last seen.
  Over 6 runners, the count and a bar of grey segments, as a job group's overflow. Busy popovers put an
  "Other hosts" label above it; idle ones do not.
- A stale peer (over 5 min, `PEER_STALE_MS`) turns the row amber and its pips into
  hollow rings. When idle, a peer shows only while it is stale (unchanged).
- More than 6 runners show as a number instead of pips.

**Daemon: finished jobs carry their CI verdict.** The decision is above; the design is in §5.
- This reverses a stance written in the code. `tally_of` in `src/reconcile.rs` says
  "faber has no visibility into the job's conclusion and must not imply it does". The
  build adds an ADR that records the change and its rule: faber shows a verdict only
  when GitHub reported one, and otherwise none. Cite the ADR by slug until it merges.

**Open:** none. Every question above was answered on 2026-10-17.

## 3. Code archaeology

- `src/reconcile.rs` `OutcomeEvent` (~line 481) is already in `StateView.outcomes`,
  newest first, capped at `OUTCOME_RECENT_CAP = 50`. Its `result` measures **the
  runner container**, not CI (see `result_of`, ~6682). An ephemeral runner exits 0 even
  when CI was red, so `success` does **not** mean passed. Only `conclusion` (new) may
  drive the green and red pips. `result` drives the red ring.
- `OutcomeEvent.job_id` is the **executed** job (`run.executed_job_id.unwrap_or(run.job_id)`).
  Under demand provisioning a runner can run a different job than it was minted for.
  The group identity must describe the executed job, not the claim's minted job (§5).
- `recent_outcomes` is in memory and empty after a daemon restart. Lingering pips do not
  survive a restart. That is accepted.
- `popover.js` `groupJobs` / `groupKey` (~139) key groups as `repo/PR-n`, or
  `repo · workflow`. Outcomes must produce **the same key**, or a finished job opens a
  second row next to its own group.
- `popover.js` `groupSig` (~187) and `syncGroups` (~235) rebuild a group only when its
  signature changes, so the pulse does not restart on every tick. The signature must
  include each pip's state and the linger bucket (passed vs faded), or the fade and
  expiry never repaint. It must not include raw elapsed seconds, or every tick rebuilds.
- `popover.js` `reduce` (~18) is the reduced-motion flag. The halo and fade must be
  off under it; the CSS `@media (prefers-reduced-motion)` rule must be added too.
- `popover.css` `@keyframes fpulse` (~140) is today's running-dot pulse. Replace it
  with the halo; do not keep both.
- `popover.js` `peerLines` / `renderPeers` (~784): `peers[].runners` is a map of
  repo → count, which is summed. `PEERS_MAX = 2` stays.
- `desktop/src-tauri/src/lib.rs`: the menu-bar glyph is running over claimed — `inflight`
  entries whose `phase` is `running`, over all of `inflight`. Outcomes must never enter
  either number.
- The webview receives the daemon's JSON verbatim (#180). New fields need no Rust
  mirror in the app. An **older daemon** without `conclusion` leaves passed and red pips
  absent, and the red ring still works from `result`. Code for that; do not assume the
  field.
- `src/github.rs` `Github::job` (~1069) is the single-job fetch the claim reconcile
  already uses. `Job.conclusion` is already deserialised.

## 4. Component inventory

There is no component library; a "component" is a DOM
builder plus its `.po-*` class.

| Element | Builder / class | Status | Fidelity | Reference |
|---|---|---|---|---|
| Card, header, ⋯ menu | `.po`, `.po-head`, `#po-menu` | reuse | as-drawn | all |
| Attention rows | `.po-attn` rows | extend: full-bleed tinted band, no inner card | as-drawn | busy, paused-frozen |
| Gauges | `GaugeBar` (`ui/src/blocks/GaugeBar.tsx`) as the summary of a `GateRow` | new 2026-10-26 (replaced the 34 px ring row) | stories | `Screens/Popover` |
| Gate detail | `GateRow` (`ui/src/blocks/GateRow.tsx`) on `Disclosure` | new 2026-10-26 (#370) | stories | `Screens/Popover` `GateOpen*` |
| Hairline separator | new `.po-sep` | new | as-drawn | all online |
| Group row | `JobGroup` on `Disclosure` | extend: name · pips · age; full bleed, neutral hover / press / open (2026-10-25) | as-drawn | job-row, busy, busy-overflow |
| Pip strip | `PipStrip` (`ui/src/blocks/PipStrip.tsx`) | new; glyphs 2026-10-25; `PeerPips` retired 2026-10-26 | as-drawn | pip-glyphs, job-row |
| Overflow bar | inside `PipStrip` | new 2026-10-25 (replaced the fraction) | as-drawn | pip-glyphs, job-row |
| Compact line | `CompactGroup` (`ui/src/blocks/CompactGroup.tsx`) | new 2026-10-26 | stories | `Screens/Popover` `BusyBeyondCap` |
| Group note (wait / fail / frozen) | `.po-gwhy` | extend: red variant, "frozen for disk" | as-drawn | busy, paused-frozen |
| Open-group job line | `jobRow`, `.po-gj` | extend: name · `i/n` · elapsed; step name in `title` | as-drawn | busy |
| Peer row | `PeerRow` | name · "N runners" · age (2026-10-26; was: grey square pips) | stories | `Blocks/PeerRow` |
| "Other hosts" label | new `.po-peerlbl` | new | as-drawn | busy, busy-overflow |
| Actions row | `.po-actions`, `#po-pause`, `#po-reconcile` | extend: ghost; Resume stays primary | as-drawn | busy-overflow (hover), paused-frozen |
| Loading frame | `GaugeBar` placeholders | empty bar tracks (2026-10-26; was: 34 px ring tracks) | stories | `Screens/Popover` `Loading` |
| Offline collapse | `.po-off` | reuse | as-drawn | offline |

## 5. Components

### Pip strip (`new`)

- **What:** one mark per job in a group or per runner on a peer. Shows state at a
  glance, with no legend needed.
- **API:** `PipStrip({ pips })` (`ui/src/blocks/PipStrip.tsx`); `PeerPips` retired on 2026-10-26, when a peer became a count.
  `pips` are already ordered (§2). Over 6, it draws the count and a bar instead.
- **States:** running, frozen, waiting, done, fault, idle, overflow bar. `idle` is a
  peer's runner with no job (2026-10-31): the running pip's ring with no arc, in `--faint` at
  full strength, so it holds 3:1 (WCAG 1.4.11); in the bar, an outlined `--faint` segment. A job
  group never draws it. A hovered, pressed or open row is a neutral lift (§ Group
  row), so every mark keeps its colour there.
  In the bar, live work (running, frozen) is filled, while done, fault, waiting and idle are
  outlines (owner, 2026-11-01: the CI-failed segment, filled and 1 px taller, is gone with its
  pip). Every segment is at full strength, so it keeps 3:1 against the row
  (WCAG 1.4.11; the canvas's 45% segments fell to about 1.9:1). See
  [`pip-glyphs.png`](reference/pip-glyphs.png), [`pip-peers.png`](reference/pip-peers.png) and
  [`job-row.png`](reference/job-row.png) (2026-10-25; they replaced `pip-states-*.png`). The screen references (`busy-*`,
  `busy-overflow-*`, `paused-frozen-*`, `idle-peer-stale-*`) drew the old dots until they were
  re-rendered from the stories on 2026-11-03; the `pip-*` images win for the marks.
- **Geometry:** 9 px glyphs, 4 px gap; ring strokes 1.3 px (running 1.6 px). The bar is
  56 × 6 with 1.5 px between segments and a 1.5 px radius; outlined segments have a 1 px
  stroke inside. A segment is at least 4 px, so an outline keeps a 2 px hollow: up to 10 jobs
  get one segment each, and from 11 each run of one kind shares a segment, sized by its share
  of the jobs; the count sits 6 px before it,
  mono 11 px `--muted`. Peer squares are 7 px in the 9 px box, radius 2 px; stale ones a
  1 px amber outline.
- **Whole pixels at 1x** (owner, 2026-10-26): the straight-edged marks sit on whole pixels so
  they stay crisp on a 1x display. The pause bars are 2 × 7 px, the clock hand and the stale
  square's outline are 1 px on the half pixel, and the bar's segment edges are rounded to whole
  pixels when drawn, so a 1.5 px gap renders as 1 or 2 px. The ring and check are curves
  and stay anti-aliased. See [`pip-1x-light.png`](reference/pip-1x-light.png) and
  [`pip-1x-dark.png`](reference/pip-1x-dark.png): each story at device pixel ratio 1, beside
  the same pixels at 6×.
- **Tokens:** running `--green`; frozen `--amber`; waiting `--amber`; done `--muted`
  (owner, 2026-11-01; was: passed `--green`, CI failed `--red`); fault `--red`; peer
  `--faint`; stale peer `--amber`.
- **Motion:**
  - running: the arc turns once every 1.2 s, linear, looping. Pips are offset by 0.4 s
    each, so a row does not turn in unison. The bar does not move.
  - done: full opacity for 20 s, then fades to 0 over the last 10 s, driven by
    `outcome.conclusion_ts_ms`, or `outcome.ts_ms` for an older daemon (owner, 2026-10-19). The board's demo loop (4 s) is for illustration only.
  - Nothing else moves. Under reduced motion: the arc holds still and nothing fades; a done pip stays
    until its 30 s are up.
- **A11y:** the strip is `aria-hidden`. The group button's accessible name carries the
  facts in words: "demo/PR-2741, 3 running, 1 waiting, 1 done, 6 minutes".
- **Content:** none. The counts come from the data.

### Group row (`extend` `fillGroup`)

- Grid: name (flex, ellipsis) · pip strip · age (30 px, right-aligned, `--mono` 11 px,
  `--faint`).
- Name: 13 px / 600 `--fg-strong`. The branch is gone from the row and stays in the
  `title`.
- States:
  - rest;
  - **full bleed** (owner, 2026-10-25, #369; was: an inset rounded row filled with
    `--accent` on hover, like a macOS menu): the row spans the card edge to edge, and its
    16 px side padding lines the text up with the header. The list sets no side padding;
  - hover: `--row-hover`, a neutral lift toward white (70% white in light, 5% `--fg-strong`
    in dark). Text and pips keep their colours, like a Mail list row. Light lifts rather than
    darkens: `--faint` holds only 3.09:1 on `--bg`, so a darker row would take it under its
    3:1 floor (2026-10-26; `tests/design_tokens.rs` checks every text role on both tints);
  - pressed: `--row-press` (white in light, 7% in dark). [`job-row.png`](reference/job-row.png)
    predates this: it holds for the row's states and layout, but its light rows darken and its
    bar's CI-failed segment is not yet taller. The tokens and the `Disclosure` stories win, as
    captured in [`row-states.png`](reference/row-states.png) (2x, both themes, 2026-10-26);
  - focus-visible: today's amber outline, inset 2 px inside the row (a full-bleed row has no
    room outside it);
  - open: the row and its job lines share `--row-hover` as one block, square;
  - all waiting: no note. The clock pips say it waits; the open row's job lines say why
    (owner, 2026-10-31; was: an amber "waits for …" note always);
  - failed only: red note;
  - frozen: "frozen for disk" note.
- **Age:** `fmtUptime` of the oldest live job. A group with only finished entries
  shows the time since its newest outcome.
- Click toggles the job lines. `aria-expanded` stays as today.

### Open-group job line (`extend` `jobRow`)

- Grid: job name · `i/n` (mono 11 px, `--faint`) · elapsed (30 px). Inset 32 px from the card
  edge (16 px past the group's name), 16 px on the right. Fill `--row-hover`, shared with the open row.
- A waiting job shows "waits for memory" (from `waiting.detail`, shortened) in amber
  instead of `i/n`. A frozen job shows "frozen for disk".
- A finished job inside an open group is listed with its outcome word: "done" in `--faint`
  (CI passed or failed; the verdict is in its `title`), or "runner failed" in red.
- `title` = the existing identity string plus the live step name.

### Gauge bar (`new` 2026-10-26, `GaugeBar`)

- **What:** one gate in force: what it is, its real figures, and how full it is. Replaces the
  34 px ring (`RingGauge`, retired), on both widths.
- **Grid:** a top line of caption (flex, ellipsis) and figure (right, no wrap), baseline
  aligned; 4 px under it, the bar, the full width of the row. Rows 6 px top and bottom.
- **Caption:** the gate's name in `small`, `--muted` ("Slots", "Memory", "Memory p75", "CPU",
  "Disk"). On the binding gate the name stays and "binds" follows it as one phrase
  ("Memory binds", no separator), the whole caption semibold, `--accent-text`, or `--amber`
  while something waits (owner 2026-10-26: the open gate must still say which gate it is).
  When the column is tight the name truncates and "binds" never does ("Memory … binds",
  `GaugeBar`'s `BindingLongCaption` story).
- **Figure:** mono `caption`, `--fg`: `3 / 6`, `5.5 / 8.0 GiB`, `3.0 / 8.0 cores`,
  `120 GiB spare` (disk floor on) or `400 GiB free` (off). A CPU gate not in `auto` reads
  `3.0 / 8.0 cores, not gated` in `--faint`.
- **Bar:** 6 px tall (`h-gauge`), radius 1.5 px, track `--border`. Fill `--accent` on the
  binding gate, `--amber` while something waits there, `--faint` when not gated, `--muted`
  otherwise. Slots draw one segment per slot, 2 px apart, a running slot filled. The **fit
  mark** is a 2 px `--fg-strong` tick across the bar's full height and no further.
- **States:** default, binding, binding while something waits, not gated, slots, slots
  binding, no fit mark, loading (an empty track, the caption, and a `--surface-2` stub where
  the figure goes).
- **Padding:** none of its own; the gate row that holds it sets 6 px top and bottom.
- **A11y:** `role="img"` with the gate's facts in words, as the ring had
  ("Memory: 5.5 of 8.0 GiB reserved, binds"). In a gate row the same words are the row
  button's name.
- **Hover:** the gate row's neutral lift (`--row-hover`, as a job group). The track stays
  `--border`, which shows on the lift in both themes, and the fills keep their colours.

### Gate row (`new` 2026-10-26, `GateRow`)

- **What:** one gate as a row that opens in place: the `GaugeBar` is the summary, and the
  open content says what the bar is made of. Built on `Disclosure`, so it is full bleed with
  the job group's hover, press, open and focus states.
- **Open content:** one line per fact, label (`small`, `--muted`, flex, ellipsis) and value
  (mono `caption`, `--fg`, no wrap), 2 px between lines, inset 16 px like the row's text.
  The wait line is a sentence after the list, `--amber`. Then **Adjust limits**, a `sm` `link`
  button. Disk lines are whole GiB, as the disk bar's figure is.
- **Content:** `gateFacts` in `ui/src/popover/gauges.ts`, only from fields present (see
  `popover.md` § Hero for the lines). Units are written out: `2.0 GiB`, `1.5 cores`.
- **States:** closed, hover, open, open while jobs wait, focus-visible.
- **A11y:** the row button is named by the gate's words and has `aria-expanded`; the figures
  are a `dl`, and the wait line a paragraph after it.
- **Motion:** none.

### Compact line (`new` 2026-10-26, `CompactGroup`)

- **What:** a job group folded past the gauge column's height, on one line: name, one pip for
  its worst state, age.
- **Grid:** the group row's columns: name (flex, ellipsis) · pip · age (`w-age`). 16 px side
  padding, so names line up with the group rows above; 2 px top and bottom.
- **Name:** `small`, `--muted`, the workflow after a `Pipe`, as the group row.
- **Pip:** one `PipStrip` glyph, the first present of fault, frozen, waiting, running, done
  (`worstPip` in `ui/src/popover/groups.ts`). It moves like the full row's (a running
  arc turns, a done check fades).
- **States:** running, waiting, fault, frozen, done.
- **A11y:** `role="group"`, named by the full row's label ("demo/PR-2760, 1 running, 2
  minutes"); `title` the full row's title. Not pressable: no hover fill, no button role.

### Peer row (`extend` `renderPeers`; pips again since 2026-10-31)

- Name `peer <instance>` in `--muted` 500 · a `PipStrip`: a `running` pip per busy runner, then
  an `idle` pip per idle one, offline runners none (count and bar past six) · age
  `fmtAgo(last_seen)`. From `PeerView.busy` / `PeerView.idle` (`src/instance.rs`), counted from
  the listing's `busy` and `status`. With no runner, `no runners` in `small` `--faint`. The label
  says the counts in words: "peer c3d4, 5 runners, 2 busy, seen 40 seconds ago" (owner,
  2026-10-31; was: the count in words, no pips, from 2026-10-26).
- Older daemon (no `busy` / `idle`): every runner is `idle`, a count that claims no work.
- Stale: the whole row turns `--amber`, and `not seen` in `small` replaces the pips: after five
  minutes nobody knows which runners exist. The label says the same, keeping the last count as
  history: "peer c3d4, not seen for 7 minutes, last seen with 2 runners". Not clickable: no hover fill, no button role.

### Actions row (`extend`)

- `.po-actions` loses its button chrome; it is the last row, with a hairline above.
- Pause: a ghost text button with the pause icon. Reconcile: a ghost icon button with
  `title` "Reconcile now", pushed right. Hover or press: `--surface-2`.
  Focus-visible: today's outline.
- Paused: `setPause` keeps rendering Resume as `.primary`, the filled accent. See
  [`paused-frozen-*.png`](reference/).

### Daemon change (`src/reconcile.rs`, `src/github.rs`)

- **Add to `OutcomeEvent`** (all `#[serde(default)]`, all optional in JSON):
  - `pr`, `branch`, `workflow`, `job_name`, `event`: the executed job's identity, in
    the same shapes as `InflightView`, so the popover keys a finished job into its
    live group;
  - `conclusion`: GitHub's verdict as GitHub spells it (`success`, `failure`,
    `timed_out`, `cancelled`, …), or absent.
- **Identity source:** the claim's identity when the executed job is the minted job.
  Otherwise use the executed job's own fields from the conclusion fetch: `name` and
  the run's workflow and branch, with no PR. It then lands in the `repo · workflow`
  group. Never attach the minted job's PR to a different job.
- **Conclusion fetch:** after the runner exits, `Github::job(repo, job_id)` once. If the
  job is not `completed` yet (GitHub lags the container by seconds), retry on the next
  reconcile cycles, at most 3 tries, then give up and leave `conclusion` absent. A fetch
  error counts as a try and is never a verdict. Update the event in `recent_outcomes` in
  place, without re-pushing it. Cost: about one ETag-cacheable GET per finished job.
- **Do not change** `result`, `tally_of`, `Counts`, or the audit record; this is display
  data only. The menu-bar glyph and admission ignore outcomes.
- **ADR:** state the reversal of the `tally_of` stance, the rule "a verdict is shown
  only as GitHub reported it", the absent-field back-compat, and the 3-try budget.
- **Tests (`cargo test`, unit, in `reconcile.rs mod tests`):**
  - identity is copied from the claim when executed equals minted;
  - identity has no PR when they differ;
  - a not-yet-completed job retries, then gives up after 3 tries;
  - a fetch error leaves `conclusion` absent;
  - each conclusion is passed through verbatim;
  - the serialised JSON has the new fields as `null` when absent.

## 6. Composition

**Two columns** (620 px, `w-popover-wide`; any job group shows), owner 2026-10-26:

1. Header.
2. Attention band rows: full bleed across both columns, no gap between them, a hairline
   under the last.
3. The columns, with a hairline under them:
   - **gauge column**, 240 px (`w-gauge-col`), a hairline on its right: the gate rows, full
     bleed to the column, 1 px apart, 6 px above and below the stack; then, when any,
     "Other hosts" and the peer rows; then the VMs line. An open gate makes the column taller,
     and the jobs column folds at its new height.
   - **jobs column**, the rest, 6 px top and bottom, no side padding (rows are full bleed to
     the column): group rows with a 1 px gap; when they fold, a hairline, the compact lines,
     and the `FoldToggle` "Show N more" ([`fold.md`](fold.md)).
4. Actions row: 6 px × 8 px.

**One column** (308 px, `w-popover`; no job group): header, attention band, the same gate
rows, a hairline inset 16 px, "Other hosts" and peer rows, the VMs line, the
actions row. The loading frame, offline, the fallback and the Limits view are 308 px too.

**Folding the jobs column.** The groups fold at the gauge column's height, so jobs never make
the card taller (`fitJobs` in `ui/src/popover/wiring.ts`):
- when compact lines let every group show, they all show, as many in full as that allows;
- otherwise groups show in full while they leave room for two compact lines and
  "Show N more", and compact lines fill what is left before it.

The first group always shows in full. An **open** group stays in full: the column grows to hold
it, up to the window's cap.

**The window** (`desktop/src-tauri/src/lib.rs`) is the card plus 36 px a side for the shadow:
380 or 692 px. The webview reports the card's width with its height (`resize_popover`), and
the app keeps the window's right edge where it is, so the beak stays under the menu-bar icon
when the card widens or narrows.

The card auto-heights (`resize_popover`) and there is no internal scroll. Order of
groups: live groups by oldest job first (as today), then finished-only groups by newest
outcome first. *Amended 2026-10-31:* live groups holding a job admitted here (running or
frozen) come before live groups that only wait, oldest first within each. Under demand
provisioning most waiting claims are the shared queue, older than the work, so oldest-first
alone folded the job the menu-bar count names into "+N more".

*Amended 2026-10-19 (owner):* the window is capped at 800 px (`resize_popover`). When the groups
would push the card past it, the popover shows as many groups as fit, then a "Show N more"
line, the same pattern as the attention list's (since 2026-10-31 both open in place:
[`fold.md`](fold.md)). The
header, gauges, attention list and actions row always show, and there is no internal scroll
until the operator opens a fold (`fold.md`). Story: `Screens/Popover` `BusyBeyondCap`. Since 2026-10-26 the gauge column's height
folds the groups first, and the cap still bounds an open group.

## 7. Out of scope

- The full panel (`index.html` / `main.js`) and the web dashboard keep their current job
  lists. Pips there are a separate decision.
- The ⋯ menu, the attention list's three-row cap and its fold line are unchanged and not
  redrawn here.
- The menu-bar glyph is unchanged.
- Making lingering pips survive a daemon restart.
- A light/dark acceptance in the **built** app is part of this work. Pixel-level visual
  regression is not: there is no harness, so acceptance is
  manual against `reference/`.

## Acceptance

Build a stub harness under `desktop/preview/`: `popover.html` plus a
`window.__TAURI__` stub and one fixture per reference artboard. Use `owner/demo`-style
names only. Capture each state at 308 px in light and dark. Compare against
`reference/<artboard>-<theme>.png` using this checklist:
inventory, order, tokens, states, a11y, content edges.

Then check the real app against a live daemon once (#180). The bar is structural and
token fidelity, not pixel identity.
