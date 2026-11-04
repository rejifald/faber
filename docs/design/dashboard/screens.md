# Dashboard screens (step 3) — anatomy

Reference: `./reference/B-Overview.png`, `./reference/B-Activity-A1.png`,
`./reference/B-Memory-M1.png` (frozen 2026-10-28); Maintenance
has no canvas artboard, so its reference is the build as approved (S13–S16), captured from its
stories: `./reference/B-Maintenance-running.png`, `./reference/B-Maintenance-attention-light.png`,
`./reference/B-Maintenance-attention-dark.png`, `./reference/B-Maintenance-drain-confirm.png`
(frozen 2026-10-28) · Decided:
[ADR-0054](../../adrs/0054-the-dashboard-is-views-in-a-sidebar.md) (five views), Activity A1,
Memory M1, owner 2026-10-28. Components: [`anatomy.md`](anatomy.md) (step 1, merged in #413).

Re-rendered on 2026-11-03 from the Storybook stories, with illustrative data; the frozen originals
stay with the legacy repository.

`B-Navigation.png`, which was frozen with the references above, is no longer in this tree, since no
story renders it; it compared three navigation layouts, which [`anatomy.md`](anatomy.md) describes
in words, and the original stays with the legacy repository.

Step 3 replaces both old full UIs at once, with nothing left behind (owner, 2026-10-28): the
daemon's vanilla page at `/` (`INDEX_HTML`, `src/web.rs`) and the app's panel
(`desktop/src/index.html`, `main.js`, `settings.js`, `styles.css`). Overview, Activity and Memory
ship together, then both are deleted. Repositories stays out of the sidebar until its own step
(ADR-0054 § 2). Resources (ADR-0054, amended 2026-10-30) has its own anatomy,
[`resources.md`](resources.md).

## 1. The screens

- **Who:** the operator of one faber host, on the host itself (the app's window) or in a
  browser through `faber dashboard`. Linux hosts have no app, so the browser is their only full
  UI.
- **Data:** `GET /api/state` (`DaemonState`, `ui/src/popover/types.ts`), polled every 2 s like
  the old page. Actions are the daemon's `POST /api/*` routes, behind the session
  (`X-Faber-Session`, ADR-0028 addendum C).
- **Entry points:** `/` (Overview), `/activity`, `/jobs`, `/memory`, `/maintenance`, `/settings`, each a flat path served by
  the React shell. The app's window and *Open dashboard* open `/`.
- **Primary viewport:** 1280 px wide, as drawn. The narrowest supported width is the `narrow`
  breakpoint (640 px), where the sidebar becomes a Select.
- **Sparsest real data:** a host that has just started. No outcomes, no classes, no peers, no
  alarms, counts all zero. Every panel needs a sentence for its empty state.

## 2. Decisions (confirmed 2026-10-28)

- **S1 → Overview's stat tiles are since-start counts.** The daemon serves totals since start
  (`counts`), not 24-hour windows or series. Each tile shows the count, a note "since start,
  11h", and no sparkline. *Wait to admit* is the p95 of `wait_s` over the outcomes held in
  memory. 24-hour series: #417.
- **S2 → The Host panel** shows Docker, Disk, Memory pressure and Uptime as drawn, plus Keep
  awake (`awake`) and the audit log path (`audit_path`), both from the old page. The drawn
  *Reclaim now* is dropped: there is no endpoint for it.
- **S3 → Pause ▾ holds Drain and Hold Docker…** (SplitButton menu). Hold asks for its reason in a
  new kit **Dialog** (Base UI Dialog), replacing the old page's `prompt()`. While a hold is set,
  Release is the alarm's action, as on the old page.
- **S4 → Activity has no range control.** The daemon keeps the most recent events of each kind
  in memory (`RECENT_CAP` 50, `DRIFT_RECENT_CAP` 25, `BYPASS_RECENT_CAP` 25 in
  `src/reconcile.rs`). The footer says "the last N events this daemon kept since it started".
  Persistent history: #418.
- **S5 → Activity's kinds** are Outcome, Decision, Fault, Retry, Drift, Bypass and Refused.
  Refused means jobs refused right now: `refused_queued`, `dind_refused` and `service_refused`,
  each with its reason. The Actor facet comes from decisions, the only events that carry one.
- **S6 → Memory serves peaks.** Each class in `/api/state` gains its window of measured peaks and
  OOM flags (a limiter change in this step). M1 then ships as drawn: the measured-peak column,
  the bimodal flag and filter, the peak histogram and the last-runs strip.
- **S7 → Memory gains two panels under the classes:** *Limits*, a KeyValueList of reserve, CPU,
  disk, machine share and cotenant, each row hidden when its field is absent; and *What parked
  jobs*, a DataTable of gate, parked now, arrivals, time parked and bound. Budget warnings are
  alarms on Overview and also sit at the top of Memory.
- **S8 → No alarm lives only in a view** (ADR-0054 § 3). Overview's *Needs attention* lists
  every alarm. The model is `dashboardAlarms` (`ui/src/dashboard/alarms.ts`): the popover's rows
  plus the old page's notices the popover never had.
- **S9 → The old page's header facts.** Owner, topic, profiles and the gate line are
  configuration, and Settings shows them. A gate that allows public repositories is an alarm.
  The status word (paused, draining, held, image missing) becomes the PageHeader's state and an
  alarm.
- **S10 → Stale and offline are shown.** The PageHeader's `live` dot turns amber "Stale" once the
  last good poll is older than 6 s. A 401 replaces the view with the session-ended state that
  Settings already uses. The old page had neither.

- **S11 → Design review answers (owner, 2026-10-28):** Activity matches A1 (regular-weight
  facets; refused and bypassed detail muted, only faults and drift tinted; an amber lead bar on
  retried and attempt-2 rows). Daemon codes read as words, unknown ones fall back to the code.
  Memory's Status shows only Pinned, Warned and Learning: a normal learned class shows no
  badge (the general rule "no bare default states", guidelines § General rules, over M1's "learned"). PeakHistogram colours
  the warm cluster amber and the cold one red, as M1 draws; RunStrip carries "oldest" and the
  last run's age. Dates use one English formatter (`fmtDateTime`), in the popover too.
- **S12 → Cancel is a link** (guidelines § Button variants), over M1's outlined Cancel.

- **S13 → A Maintenance view holds every host action** (owner, 2026-10-28; ADR-0054 § 2
  amended). `/maintenance`, a wrench icon between Memory and Settings. Overview's header loses
  Reconcile and the Pause ▾ menu; alarm cards keep the one fix they name. There is no "stop":
  Pause is the reversible stop, and Drain keeps its name behind a confirmation. Not drawn on
  the canvas; the owner approved the layout in § 4 "Maintenance" as written.

- **S14 → Maintenance review answers** (owner, 2026-10-28): the Updates panel's version row
  is "Installed"; its update-ready row says the version once; every row action is `md`, Pause
  included; a nav count that is a word (Paused, Draining, Held) is set in sans, numbers stay
  mono. The sidebar's update card follows the Updates panel and links to it.

- **S15 → Maintenance uses the width** (owner, 2026-10-28, over S13's single column): a row of
  four control tiles (Host, Docker admission, Updates, Jobs), each its state as the hero plus
  its one or two actions; below, Repository holds and Golden images side by side; Stand down
  last, one line. Below `wide` the tiles go two per row and the lists stack.

- **S16 → Maintenance review answers** (owner, 2026-10-28): healthy default states are calm
  (normal text, a quiet dot); an offered update is accent, not amber; Stand down is one row with
  no heading. Tile labels inside their box and list titles above theirs both stay, as do
  equal-height tiles with bottom-aligned buttons.

## 3. Code archaeology

- **Old page (deleted in this step):** `INDEX_HTML` in `src/web.rs`. Markup ~1725–1866, script from ~1868,
  `refresh()` ~2539. It polls every 2 s (`setInterval(refresh, 2000)`). Its sections and notice
  slots are inventoried in the step-3 PR; every one maps to a row in § 4.
- **Shell routes:** `src/web.rs` `routes()` serves `/` (`index` → `INDEX_HTML`) and `/settings`
  (`settings_shell`, the React `index.html`). The shell's asset URLs are relative, so a view is
  a flat path, never nested or slash-terminated (ADR-0054 § 5).
- **Entry:** `ui/dashboard/main.tsx` mounts Settings at `/settings` and a Placeholder elsewhere.
- **Session:** the redeem page stores `faber-key` and `faber-since` in sessionStorage
  (`src/control.rs`). `ui/src/session.ts` reads them; a 401 is `SessionEnded`.
- **Popover rows are frozen** (`docs/design/popover.md`). `dashboardAlarms` composes the
  popover's producers and must not change their output (`ui/scripts/dashboard.test.mjs`).
- **Classes:** `ClassCapView` in `src/limiter.rs`. `runner_mib` and `dind_mib` are the cap in
  force; `pinned` marks an operator pin; `samples` is the window length. Peaks are added in this
  step (S6).
- **Pins:** `POST /api/pin` (`set_pin`, `src/web.rs`). A pin is live; `overrides.json` is read at
  startup only.
- **Panel window:** ADR-0041 decisions 1 to 3 and ADR-0028 addendum C. The window loads the
  dashboard with a session the app mints, and gets no Tauri IPC. Start at login and the app's
  update already live in the popover's ⋯ menu.

## 4. Inventory

Fidelity is `as-drawn` unless a § 2 decision says otherwise.

### Shell (every view)

| Element | Component | Verdict | Fidelity |
|---|---|---|---|
| Sidebar, brand, nav with counts | `DashboardNav` `mode="page"` | reuse | as-drawn |
| Host card + peers | `HostCard` | reuse | as-drawn |
| Update card | `UpdateCard` | reuse | as-drawn |
| View title, live dot, actions | `PageHeader` | reuse | S10 adds the stale state |
| Session ended | Settings' session-ended state | reuse | — |

### Overview (`/`)

| Element | Component | Verdict | Fidelity |
|---|---|---|---|
| Reconcile | `Button` secondary `lg` | reuse | as-drawn |
| Pause ▾ (Drain, Hold Docker…) | `SplitButton` | reuse | S3 |
| Hold reason | `Dialog` | **new (kit)** | S3 |
| Memory budget | `BudgetRail` | reuse | as-drawn |
| Four stat tiles | `StatTile` | reuse | S1 (no trend) |
| In flight | `DataTable` (status dot, tone bar, repo #PR, job, step, memory, elapsed) | reuse | as-drawn |
| Latest activity + filter | `SegmentedControl` + `DataTable` with `RowDisclosure` | reuse | as-drawn; "All activity →" links to `/activity` |
| Needs attention | `AttentionCard`, one per alarm, worst first | reuse | S8; one quiet line when empty |
| Host | `Panel` + `KeyValueList` | reuse | S2 |
| Learned caps | `Panel` + `KeyValueList` (badge + value) | reuse | as-drawn; links to `/memory` |

### Activity (`/activity`, A1)

| Element | Component | Verdict | Fidelity |
|---|---|---|---|
| Search | `Input` with a search icon | reuse | as-drawn |
| Range switch | — | dropped | S4 |
| Facets (Kind, Result, Repo, Actor) with counts | `FacetList` | **new (block)** | as-drawn |
| Event table | `DataTable` + `RowDisclosure` + `StatusBadge` | reuse | as-drawn |
| Row detail: message block | `CodeBlock` | **new (block)** | as-drawn |
| Row detail: runner, attempt, profile, wait, duration, job ID | `KeyValueList` `layout="grid"` | **extend** | as-drawn |
| Row detail of a finished run: its phase bar, CPU, throttling and bytes, read from `GET /api/runs/:runner` when the row opens (ADR-0056 § 5) | `RunPhases` (block) over `PhaseBar` | **new** | not drawn |
| Footer count | text | — | S4 wording |

### Jobs (`/jobs`, ADR-0056 § 5)

Between Activity and Memory in the sidebar. Data: `GET /api/jobs?days=N` (7, 30 or 90), read from
the audit log's `outcome` lines, not from `/api/state`. Not drawn on a canvas: its reference is
the build, captured from its stories and frozen with no canvas round (owner, 2026-10-30):
`./reference/B-Jobs.png`, `./reference/B-Jobs-dark.png` and Activity's
`./reference/B-Activity-run-phases.png`.

| Element | Component | Verdict | Fidelity |
|---|---|---|---|
| Title, live dot, "N classes, M ranked in the last 7 days" | `PageHeader` | reuse | not drawn |
| Window (7, 30, 90 days) | `SegmentedControl` | reuse | not drawn |
| Lead line: ranked by total wall time; throttled time and bytes are lower bounds | text | — | not drawn |
| Classes table: class (the job name in the UI face, its repo and workflow muted in mono under it, owner 2026-10-30; Profiling badge under 20 runs), runs, wall time, where the time went, parked share and gate, CPU used / reserved, throttled, bytes | `DataTable` + `StatusBadge` | reuse | not drawn |
| Where the time went, in a cell | `PhaseBar` `compact` (kit chart, visx) | **new** | not drawn |
| Row detail: the phase bar with each median and p90, then wall, parked share, CPU, throttling, bytes, macOS runs without resources, skewed clocks | `PhaseBar` + `KeyValueList` `layout="grid"` | reuse | not drawn |
| "Reading this table" | `Panel` with text | reuse | not drawn |

The parked share turns amber at 25% (`PARKED_ATTENTION_SHARE`), a starting value to revisit once
real distributions show (owner, 2026-10-30). Anything not measured reads "not measured", never zero; throttled time and bytes are prefixed
"≥" as lower bounds.

### Memory (`/memory`, M1)

| Element | Component | Verdict | Fidelity |
|---|---|---|---|
| Header note "learned caps adjust…" | `PageHeader` | reuse | as-drawn |
| Memory budget | `BudgetRail` (headroom in the note) | reuse | as-drawn |
| Class filter (All, Pinned, Warned, Bimodal) | `SegmentedControl` | reuse | as-drawn |
| Classes table, selectable row | `DataTable` `selectedKey` / `onSelect` | **extend** | as-drawn |
| Samples tick strip | `SampleBars` (kit chart, visx) | **new** | as-drawn |
| "Reading this table" | `Panel` with text | reuse | as-drawn |
| Class detail: title, badge, close | `ClassDetail` | **new (block)** | as-drawn |
| Cap in force / measured peak / pinned override | `StatTile` `size="sm"` | **extend** | as-drawn |
| Explanation | text in `ClassDetail` | — | as-drawn |
| Peak distribution | `PeakHistogram` (kit chart, visx) | **new** | as-drawn (S6) |
| Last 14 runs | `RunStrip` (kit chart, visx) | **new** | as-drawn (S6) |
| Set a pin (runner, dind, cold-safe suggestion, Save, Cancel) | `Input` + `Button` | reuse | as-drawn |
| Limits | `Panel` + `KeyValueList` | reuse | S7 (not drawn) |
| What parked jobs | `Panel` + `DataTable` | reuse | S7 (not drawn) |

### Maintenance (`/maintenance`, S13–S16)

Reference: `B-Maintenance-running.png` (calm defaults), `B-Maintenance-attention-*.png` (draining,
held, a failed update, a repository hold and a failed golden image, in the shell), and
`B-Maintenance-drain-confirm.png`. A change to this view updates these images in the same change.

A row of four ControlTiles, then two lists side by side, then one Stand down row (S15). Each
tile leads with a state, short (a word or a phrase), with its facts a line each below and its
actions at the bottom; tiles in a row share a height, so their buttons align. A healthy default
is calm (normal text, quiet dot); colour marks only what needs the operator (S16). Every action
runs through `run`, shows `loading` on its own button, disables its siblings in the same tile or
list while it runs, and shows the daemon's own error text in place (`role="status"`). Every
row action is `md` (S14).

| Place | State / content | Action |
|---|---|---|
| Host tile | Running (calm), Paused (amber), Draining (amber, "N jobs still running") | Pause / Resume; none while draining |
| Docker admission tile | Open (calm), or Held (amber) with its reason and since | Hold… (a Dialog asks the reason) / Release |
| Updates tile | Leads with a failure (red), else "X available" (accent), else the phase or "Up to date" (calm); below: "Installed" and the version in mono, the phase, the why, the last check | Install now (the page's only primary, only when on offer), Check now, Skip, as the phase allows |
| Jobs tile | What is in flight: "N running, M parked", or "No jobs in flight" (calm) | Reconcile, with a line saying what it does |
| Repository holds list | One row per `repo_holds[]`: repo, reason, since, by; "No repository is held." when empty | Clear (a 403 shows the copyable `sudo faber repo-hold clear` command) |
| Golden images list | One row per `budget.vm.golden[]`: image, standing in words; the list is absent when no golden image is configured, and Repository holds takes the width | Verify now |
| Stand down row | One row, no heading: "Drain", then what it does, on one line | Drain… (`tone="danger"`), confirmed in a Dialog that names the running jobs (confirm is `danger`, Cancel is a link); absent while draining |

The sidebar's UpdateCard summarises the Updates tile, shown only when Install is on offer, and
links here.

## 5. New and extended components

### Dialog (kit, new)
- **For:** a short form that must be answered before an action runs (Hold Docker's reason).
  Not for read-only content (use Popover).
- **Built on:** `@base-ui/react/dialog`.
- **Props:** `open`, `onOpenChange`, `title`, `description?`, `children` (the form),
  `actions` (Buttons; one `primary`).
- **States:** closed, open, submitting (the primary is `loading`), error (`role="status"`
  line).
- **A11y:** focus moves to the first field. Escape and the backdrop close the dialog; focus
  returns to the trigger. It is titled by `title`.
- **Tokens:** popup `rounded-lg bg-elevate border border-border shadow-floating p-xl`, width
  `w-popover`; backdrop is the `bg` role at partial opacity (a theme token, no raw colour).

### FacetList (block, new)
- **For:** Activity's filter column: groups of values, each with a count, any number checked.
- **Props:** `groups: { id, label, items: { value, label, count, tone? }[] }[]`,
  `selected: Record<groupId, string[]>`, `onChange`.
- **States:** default, item hover, item selected, focus-visible, a group with no items (hidden).
- **A11y:** each group is a `group` named by its label; items are toggle buttons with
  `aria-pressed`.
- **Tokens:** card `rounded-lg border bg-surface p-lg`; group label `text-caption font-semibold
  text-muted`; item `text-small`; count `font-mono text-caption text-faint`; a tone shows a
  `StatusDot`.

### CodeBlock (block, new)
- **For:** a multi-line message from the daemon or GitHub, set as text.
- **Props:** `children: string`.
- **Tokens:** `rounded-md bg-bg border border-border-soft p-md font-mono text-small text-fg
  whitespace-pre-wrap wrap-anywhere`.

### KeyValueList (extend)
- Add `layout?: 'rows' | 'grid'`. `grid` sets label-over-value pairs in columns
  (`text-caption text-faint` label, `font-mono text-small` value), as drawn in A1's open row.

### DataTable (extend)
- Add `selectedKey?: string` and `onSelect?(key)`. A selectable row is pressable as a whole, the
  selected one gets `bg-row-press` and an accent lead bar, and the table is announced as a grid
  with `aria-selected`.

### StatTile (extend)
- Add `size?: 'md' | 'sm'`. `sm` is the class detail's three boxes: title value, not display,
  plus a two-line note.

### SampleBars, PeakHistogram, RunStrip (kit charts, new, visx, ADR-0045)
- **SampleBars:** 3 to 4 bars, showing how full a class's sample window is. Amber when under the
  learning threshold. `aria-hidden`; the count beside it is the text.
- **PeakHistogram:** a class's peaks in buckets, with the cap and the cold-safe value as marked
  lines and labelled counts. It has an `aria-label` summary: "47 samples, 38 near 3.1 GiB, 9
  near 8.2 GiB".
- **RunStrip:** the last N peaks as bars, red where a run went over the cap. It has an
  `aria-label` summary.

### PhaseBar (kit chart, new, visx, ADR-0045, ADR-0056)
- **For:** where a run's or a class's time went: one horizontal bar of its phases in order, each
  as wide as its share of the measured whole.
- **Fill says whose time it was:** amber with a diagonal hatch waiting for capacity (parked; the
  hatch, an svg pattern in both themes, keeps it apart from the accent, owner 2026-10-30), `faint` and `muted` for
  GitHub's and faber's overhead, the accent for the job's setup steps, `fg` for its job steps,
  `border-control` for post steps. A 2 px gap separates segments, so two of one tone read as two.
  No new colour role.
- **Unmeasured:** a phase with no figure is left out of the bar and listed as "not measured". A
  phase whose clocks disagree reads zero, and the legend says "clocks disagree".
- **Compact:** a thin bar with no legend, for a table cell; its `aria-label` lists each phase.

### RunPhases (block, new)
- **For:** Activity's finished-run row detail. Reads `/api/runs/:runner` when the row opens.
- **States:** loading, measured, partly measured, not recorded (a run older than the fields),
  failed.

### ClassDetail (block, new)
- **For:** Memory's side panel for the selected class.
- **Props:** `cls` (a class view model), `onClose`, `onPin(runnerMib, dindMib)`, `pending?`,
  `error?`.
- **States:** learned, pinned, warned, bimodal, still learning (fewer than 15 samples), saving a
  pin, pin failed.
- **Composition:** the drawn order: title + badge + idle line; three `StatTile sm`; the
  explanation (bimodal or warned wording); PeakHistogram; RunStrip; Set a pin.

## 6. Composition

- **Shell:** as `anatomy.md` § 6. The sidebar is `w-sidebar`; the main column scrolls.
- **Overview:** header → budget rail → four tiles in a row → two columns: on the left In flight
  and Latest activity; on the right (`w-rail`) Needs attention, Host and Learned caps. Section gap
  `2xl`.
- **Activity:** header (title, live dot, then the count as the header's `meta`; search right) → two columns: FacetList (`w-facets`,
  190 px as drawn) and the table, which takes the rest.
- **Memory:** header → budget rail → "Classes" heading with the filter right → two columns: table
  plus "Reading this table" on the left; ClassDetail (`w-detail`, 400 px) on the right while a class is
  selected → Limits → What parked jobs.
- **Below 1280 px (`wide:`):** the right column drops under the left. Below `narrow`, facets collapse into
  a disclosure above the table, and the sidebar becomes a Select.
- **States per view:** loading (first poll: skeleton-free, the header says "Connecting…"),
  stale (S10), session ended, empty (a sentence per panel).

## 7. Out of scope (step 3)

- Repositories (R1/R2) and its daemon endpoint (#421).
- 24-hour series and sparklines (#417), and history beyond memory and a range control (#418).
- Light-mode references: the artboards are dark only. Light is the theme's job and is checked
  in stories.
