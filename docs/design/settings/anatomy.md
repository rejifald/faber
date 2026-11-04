# Settings: popover limits strip and the dashboard Settings view, anatomy

Reference: [`./reference/`](reference/) (frozen 2026-10-22) · Decided: 2026-10-22 by the
owner, from three rendered directions · Canvas: the owner's design canvas, not published,
which also keeps the rejected directions

This is the build spec for the settings redesign. A builder reads only this file and the
reference images. Anything this file does not state is not licensed.

It ships in **two PRs**, decided by the owner on 2026-10-22:

- **PR 1, the popover limits strip:** a Limits view inside the React popover for the live
  autolimiter knobs. It uses the Tauri commands the popover already has.
- **PR 2, the Settings view:** the rest of `config.toml`, profiles included, as the first
  React screen of the dashboard (ADR-0041), served at `/settings` behind the session. It
  retires the panel's Settings tab (`desktop/src/settings.js`) and the inline dashboard's
  Settings tab (`src/web.rs`), so Settings has one form.

| Reference image | What it shows | PR |
| --- | --- | --- |
| `Main.png` (removed) | The chosen hybrid: the strip, an arrow, the Settings view on Docker host | 1, 2 |
| `D1-Allocation.png` (removed) | Strip entry from the gauges (a), the strip (b), its states (c) | 1 |
| `D3-Window.png` (removed) | Settings view on Limits and Admission tuning, the save bar, search, states | 2 |
| `Profiles.png` (removed) | Profiles list (a), one profile after Revoke (b) | 2 |
| `ProfilesStates.png` (removed) | New, remove, refused save, macOS VM fields, native profile (removed, ADR-0052), empty | 2 |
| `Dark.png` (removed) | The strip and one profile in the dark theme | 1, 2 |
| [`Trust.png`](reference/Trust.png) | The Trust section, lists narrowing in place, and their states | 2 |
| `Lists.png` (removed) | Environment, Dropped grants, Priority repos, and their states | 2 |
| [`Raw.png`](reference/Raw.png) | config.toml as text, and its states | 2 |

`Main.png`, `D1-Allocation.png`, `D3-Window.png`, `Profiles.png`, `ProfilesStates.png`, `Dark.png`
and `Lists.png` are no longer in this tree: each was a canvas composite of several artboards, so
no single story renders it, and the originals stay with the legacy repository. The Fidelity lines
below keep citing them by name; the built states are in the `Screens/Settings` and
`Screens/LimitsView` stories. `ProfilesStates.png` drew six panels: a new profile whose name
duplicates an existing one, refused with "A profile called linux already exists."; Remove
confirmed in place ("Remove linux?", with Cancel and Remove); a save refused because it widens
trust, with the `sudo faber config apply config.toml` command, Discard and Copy command; the
macOS VM fields (VM vCPUs and VM memory); a native profile, ignored; and the empty state, with
Add profile. `Trust.png` and `Raw.png` are the originals.

The artboards draw the Settings view inside a window frame titled "faber settings". The frame
is the canvas's shorthand for "the panel window" and is not built: the view is a page of the
dashboard (§ 2).

## 1. The screens

### The limits strip (PR 1)

- **Purpose:** change how much of the engine faber admits jobs into, from the menu bar, in one
  click and without a save step.
- **Who:** the operator, from the tray popover.
- **Data:** `snap.state.budget` (`LimiterView`, `src/limiter.rs:3506`): `budget_mib`,
  `reserved_mib`, `reserve_mib`, `headroom_mib`, `margin`, `native_concurrency`,
  `hard_ceiling` (resolved number or `null`), `hard_ceiling_setting` (`"auto"`, `"off"` or a
  number), `docker_inflight`, `headroom {jobs, binds}`.
- **Writes:** the `set_config` Tauri command, which posts `POST /api/config`
  (`src/web.rs:306`) with any subset of the five knobs. The daemon applies them live and keeps
  them in `overrides.json`.
- **Entry points:** *Adjust limits* in an open gate of the popover (since 2026-10-26, #370; was: the gauge row as a button, owner 2026-10-22), and a *Limits…*
  item in the ⋯ menu.
- **Viewport:** the popover's fixed 308 px card, light and dark. No breakpoints.
- **Sparsest data:** budget present with nothing running (`reserved_mib` 0, so the Jobs segment
  is absent). A daemon too old to send `budget`, or an offline daemon, shows the unavailable
  state.

### The Settings view (PR 2)

- **Purpose:** edit the whole `config.toml`, and say plainly which change applies now and
  which waits for a restart.
- **Who:** the operator, in the app's panel window or in a browser on a host with no app
  (Linux).
- **Data:** `GET /api/settings` (`src/web.rs:656`, `SettingsView` at :541): `config` (with
  `profile[]`), `defaults`, `pending_restart[]`, `live_overrides?`, `path`, and `schema
  {sections[], profile_fields[]}` (`src/settings.rs:143-158`). Every field renders from its
  `FieldSpec`; no key is hard-coded in the UI.
- **Writes:** `POST /api/settings` with the whole `FileConfig`, `?base=<digest>` from the last
  read (a stale base answers 409). The reply is `SaveResult` (`src/web.rs:631`).
- **Entry points:** *All settings…* in the strip, *Settings…* in the ⋯ menu, ⌘, while the
  popover is focused, and the dashboard's own link to `/settings`.
- **Viewport:** the panel window (460 × 640 today, minimum 380 × 460) and a browser tab. The
  view is fluid; § 6 says what happens below 640 px.
- **Sparsest data:** a config with no profiles (the empty state), and a daemon run with
  flags and no config file (`from_flags`). It has no profile, so it claims no jobs until a save
  writes a file with one and faber restarts (ADR-0052).

## 2. Decisions

All by the owner on 2026-10-22.

- **The hybrid:** the live knobs in the popover, everything else in one Settings view. Rejected:
  the whole config drilled into the popover (direction 2), because the popover closes on blur
  and loses a half-typed field, and 15-field profiles and multi-line lists are cramped at 308 px.
  Rejected: settings only in a window with a read-only popover, because allocation was the thing
  the owner wanted one click away.
- **The strip applies each change at once.** No Save, no Discard. A row that fails keeps its
  error and a Retry; nothing else on the strip is blocked.
- **The Settings view is a dashboard page, not a third app window.** ADR-0041 makes the
  dashboard the one full UI and Settings one of its views; ADR-0028 addendum C makes the panel
  window a browser tab onto it, with no Tauri IPC. A Settings window with IPC would be a second,
  more privileged way into the daemon.
- **Served at `/settings` before the rest of the dashboard moves** (owner, 2026-10-22). The React
  shell answers `/settings` behind the session; `/` stays the inline dashboard until its
  screens move. PR 2 amends ADR-0028 addendum C by one line ("the shell answers `/` and
  `/settings`") and builds its decision 2 (`/assets/*` without a session). PR 2 is a security
  slice: it needs a security review on its final head.
- **The save bar is raised** in both themes: `--elevate` fill, `--border`, `--shadow-float`.
  Rejected: the dark filled bar first drawn in the light theme, which inverted the theme with
  no colour role for it.
- **No reordering of profiles in the UI.** Profile order is routing: the first profile whose
  labels a job's `runs-on` matches takes it. A move can put jobs on a profile with Docker
  access, which widens trust (`src/control.rs:478-493`), so reordering stays
  `sudo faber config apply`.
- **Add profile appends last,** so a new profile takes only jobs no earlier one matches. It
  starts as `labels: [faber]`, `backend: docker`, `dind: false` stated, not blank (a blank
  grants it; `src/web.rs:2478`).
- **Hard ceiling in the strip** (owner, 2026-10-22): the stepper shows the resolved number.
  On `auto` the hint reads "Auto, faber's pick", and the first press of − or + makes it an
  explicit number and shows a *Use auto* link button in the hint line. On `off` the value reads
  "Off", the hint "Budget only", and + sets the resolved number (or 1 when there is none).
  Setting it to `off` happens only in the Settings view.
- **Native jobs stays in the strip** (owner, 2026-10-22), five knobs as drawn, although
  ADR-0026 removed the native backend.
- **The gauge row is the strip's entry** (owner, 2026-10-22): the whole ring row is one button,
  with a hover fill, the tooltip "Adjust limits" and that accessible name. The permanent
  "Adjust limits ›" line drawn under the gauges in `D1-Allocation.png` (a) is **not built**.
  The ⋯ menu also gets *Limits…*. *Superseded on 2026-10-26 (#370):* the gates open in place,
  each with an **Adjust limits** link at the end of its detail (`popover/anatomy.md` § 5, Gate
  row); *Limits…* in the ⋯ menu stays.
- **Copy follows the React UI's separator rule.** The artboards join facts with `·`
  ("Reserve 2.0 · jobs 6.0 …", "1 unsaved · applies on restart"). `ui/` lints `·` out
  (`design-tokens/no-dot-separator`), so the build uses the swatch legend grid of
  `D1-Allocation.png` (b) for the bar, and one phrase per line elsewhere: "1 unsaved change.
  Applies on restart." This is the guidelines' rule, not a design change.
- **Steps:** Reserve and Headroom step by 512 MiB and display in GiB with one decimal; Margin
  steps by the schema's 0.05 and displays as `1.2×`, `1.25×`; Hard ceiling and Native jobs step
  by 1. The minimums are the schema's: reserve 0, headroom 1 MiB, margin 1, the counts 1.

- **Trust lists narrow in place** (owner, 2026-10-22): a `sudo` list or map is edited in the
  form in the direction that narrows it, and shows its `sudo` command for the direction that
  widens it (`Trust.png`, `Lists.png`). The daemon already refuses every widening save
  (`src/control.rs:534-578`, `:366-382`); the schema gains `narrows` so the form knows the
  direction. Rejected: keeping every `sudo` field read-only, as the old form did.

- **config.toml as text** (owner, 2026-10-22): a raw editor beside the form, as drawn in
  `Raw.png`, with four rules. (1) A save writes the operator's text as-is, comments and layout
  kept, after the daemon parses it and runs the same validation and widening check as the form;
  the form's save still re-serialises. (2) A plain text area with line numbers and error-line
  marking; no syntax highlighting, so no new dependency. (3) One draft at a time: unsaved form
  changes must be saved or discarded before editing the text, and the other way round. (4) It is
  the last item in the nav, `config.toml`, under faber.

Open: none.

## 3. Code archaeology

- `ui/src/popover/wiring.ts:16-30`: the `commands` table every popover control goes through;
  `ui/scripts/wiring.test.mjs` pins it. A new command is a new entry there, never a bare
  `invoke`.
- `desktop/src-tauri/src/lib.rs:1461`: `set_config(reserve_mib?, headroom_mib?, margin?,
  native_concurrency?, hard_ceiling?)`. Tauri maps camelCase args (`reserveMib`) to these.
  `hard_ceiling` is the string `"auto"`, `"off"` or a number. It returns the daemon's plain-text
  body on a 400 and **does not poll**, so the popover would show the old value until the next
  poll: PR 1 makes it call the same poll-and-emit the other controls rely on.
- `ui/src/popover/types.ts:76`: `Budget` lacks `reserve_mib`, `headroom_mib`, `margin`,
  `native_concurrency`, `hard_ceiling_setting`, `available_mib`. The daemon sends them; PR 1
  adds them as optional (an older daemon omits them, and the strip then shows the unavailable
  state).
- The bar's arithmetic: engine memory = `budget_mib + reserve_mib` (`src/settings.rs`'s hint:
  budget = MemTotal − reserve). Jobs = `reserved_mib`. Headroom = `headroom_mib`. Free =
  `max(0, budget_mib − reserved_mib − headroom_mib)`. The old panel bar is
  `desktop/src/main.js:354-400`; its knob panel, which this replaces, `:128-250`.
- The CPU warning on Hard ceiling: when `budget.headroom.binds` is `"cpu"` and the explicit
  ceiling exceeds `docker_inflight + headroom.jobs`, the row warns "Above what CPU allows. CPU
  binds at N." with N that sum.
- `lib.rs:1418`: `resize_popover` clamps to 80..800. `ui/src/entries/PopoverApp.tsx:23-50`
  resizes the window from the card's measured height, so a taller Limits view needs nothing
  else.
- `lib.rs:1585` `popover_closed_for` counts only the `main` window as open, so the app does not
  self-update under it. The panel window stays `main` in PR 2.
- `src/settings.rs:64-128` `FieldSpec`: `sudo` fields are read-only and round-trip as loaded;
  `narrow {label, value}` is offered only when it moves the value narrower
  (`desktop/src/settings.js:61-70` `narrowShown`); `<profile>` in a `sudo` template is replaced by
  the profile's name, quoted as one shell word (`settings.js:71-80` `sudoCmd`, mirroring
  `control::shell_quote`). An optional bool's `options` are its three labels
  `[unset, true, false]`; an optional enum gets a blank "unstated" choice. `settings.js:179-232`
  `readField` is the exact blank-value semantics PR 2 must keep.
- `src/web.rs:726` `POST /api/settings`: 200 saved, 400 validation (`errors[]`), 403 widening
  refused (`refused[{key, sentence, needs, command}]`), 409 stale base, 500 write failure. A save
  also pushes the five knobs live and clears their overrides (`web.rs:950-960`).
- `src/ui_bundle.rs`: the React dashboard is embedded but routed nowhere; its header lists what
  routing needs. ADR-0028 addendum C decision 2 is the spec for `/assets/*`.
- `desktop/src/main.js:7` is the only importer of `settings.js`; PR 2 removes the import, the
  init call, the tab buttons and the Settings markup in `desktop/src/index.html` together.
  `set-*` classes in `styles.css` may be shared with `main.js`: check before deleting.
- **PR 2, the front door** (`src/control.rs:1187-1222` `FrontDoor::admit`): `/` admits on the
  cookie alone (`NoKey if path != "/"` → 401 at :1213); every other path needs cookie and key.
  `/settings` joins `/` there. `/assets/*` becomes `Admit::Open` for `GET`/`HEAD` only, decided on
  the raw path the router matches; a mutation there falls through to 401/403/405 as addendum C
  says. The socket router has no `admit`, so both routes also answer there: harmless.
- **PR 2, sessions** (`src/control.rs:1463` `mint`, `:1493-1530` `redeem`): the redeem page stores
  `faber-key` and `faber-since` in `sessionStorage` and `location.replace('/')`. PR 2 adds a
  `next` query value matched against an allowlist (`settings` → `/settings`, anything else →
  `/`), never echoed. `dashboard_link::is_session_url` (`desktop/src-tauri/src/dashboard_link.rs:22-37`)
  and `src/dashboard.rs` validate the exact link shape and must accept the one optional
  `&next=settings` suffix. The React page reads the same two `sessionStorage` keys and sends
  `X-Faber-Session`, as `INDEX_HTML`'s `apiFetch` does (`src/web.rs:1334-1349`).
- **PR 2, authority:** a dashboard session is `Caller::Session`, class `Operator`
  (`src/control.rs:97-101`); `authorize` (`:160-190`) refuses any widening below `Present` with
  403 and `refused[]`. No HTTP caller is ever `Present`, so the form can narrow but never widen,
  whatever it sends.
- **PR 2, the bundle:** `build.rs:57-119` embeds `ui/dist/dashboard` as `ui_bundle::FILES`, keys
  `index.html` and `assets/<hash>.<ext>`. Vite's `base: './'` resolves `./assets/x.js` from
  `/settings` to `/assets/x.js`, so it stays; the shell must never be served under `/settings/`.
- **PR 2, the old forms:** the inline dashboard's Settings tab is `src/web.rs:1182` (button),
  ~1306-1323 (view), 2126-2466 (JS); `#notices` and `#credential` (1170-1178) sit above both tabs.
  The panel's is `desktop/src/settings.js` via `main.js:7`.
- ESLint (`ui/eslint.config.mjs`, `ui/eslint-rules/design-tokens.mjs`): no raw colours, no
  arbitrary Tailwind values, no `style` prop or raw `<button>`/`<input>` outside the kit, no
  `@base-ui/react` import outside the kit, control sizes only through the kit's `size`.

## 4. Component inventory

| Element on artboard | Component | Status | Import | Fidelity | Artboards | PR |
| --- | --- | --- | --- | --- | --- | --- |
| Gauge row as a button (superseded 2026-10-26: `GateRow`'s Adjust limits) | `GaugeButton` (in `Popover.tsx`) | extend | `ui/src/screens/Popover.tsx` | diverges → § 2 (no extra line) | D1 a | 1 |
| Strip header: back, title, "Applies now" | `ViewHeader` | new | `ui/src/blocks/ViewHeader.tsx` | as-drawn | D1 b, Main, Dark | 1 |
| Engine memory bar and legend | `MemoryBar` | new | `ui/src/blocks/MemoryBar.tsx` | as-drawn (legend grid, § 2 copy rule) | D1 b, Main, Dark | 1 |
| − value + control | `Stepper` | new | `ui/src/kit/Stepper.tsx` | as-drawn | D1 b c, Main, Dark | 1 |
| Knob row with status line | `LimitRow` | new | `ui/src/blocks/LimitRow.tsx` | as-drawn | D1 b c, Main, Dark | 1 |
| The strip | `LimitsView` | new | `ui/src/screens/LimitsView.tsx` | as-drawn | D1 b c, Main, Dark | 1 |
| "All settings…" footer | `Button variant="link"` | reuse | `ui/src/kit` | as-drawn | D1 b, Main, Dark | 1 |
| Retry, Undo, Turn off, Clear, Discard, Add profile | `Button variant="secondary" size="sm"`/`md` | reuse | `ui/src/kit` | as-drawn | D1 c, D3, Profiles | 1, 2 |
| ⋯ menu *Limits…*, *Settings…* | `MenuItem` | reuse | `ui/src/kit` | added items, § 2 | none | 1, 2 |
| Section sidebar with groups and search | `SettingsNav` | new | `ui/src/blocks/SettingsNav.tsx` | as-drawn | D3, Profiles, Main, Dark | 2 |
| Search field | `Input type="search"` | reuse | `ui/src/kit` | as-drawn | D3 | 2 |
| Section title and hint | `SectionHeader` | new | `ui/src/blocks/SectionHeader.tsx` | as-drawn | D3, Profiles | 2 |
| Field row: label, hint, live/restart tag, control | `FieldRow` | new | `ui/src/blocks/FieldRow.tsx` | as-drawn | D3, Profiles, Main | 2 |
| Text and number inputs | `Input` | reuse | `ui/src/kit` | as-drawn | D3, Profiles | 2 |
| Enum dropdown | `Select` | reuse | `ui/src/kit` | as-drawn | D3, Profiles | 2 |
| Toggle switch | `Switch` | new | `ui/src/kit/Switch.tsx` | as-drawn | Profiles, Main, Dark | 2 |
| Read-only trust row: value, sudo command, copy, narrowing | `TrustRow` | new | `ui/src/blocks/TrustRow.tsx` | as-drawn | Profiles, Dark | 2 |
| List of items, edited per item | `ListField` | new | `ui/src/blocks/ListField.tsx` | as-drawn | Trust, Lists | 2 |
| Name and value rows | `MapField` | new | `ui/src/blocks/MapField.tsx` | as-drawn | Lists | 2 |
| Copy button | `IconButton` | reuse | `ui/src/kit` | as-drawn | Profiles | 2 |
| Raised save bar | `SaveBar` | new | `ui/src/blocks/SaveBar.tsx` | as-drawn (raised, § 2) | D3, Profiles, Main, Dark | 2 |
| Profile list row | `ProfileRow` | new | `ui/src/blocks/ProfileRow.tsx` | as-drawn | Profiles a, ProfilesStates | 2 |
| Breadcrumb "Profiles › name" + Remove | `ViewHeader` variant `crumb` | extend (PR 1's) | `ui/src/blocks/ViewHeader.tsx` | as-drawn | Profiles b, Dark | 2 |
| Inline notices: restart pending, not saved, refused, can't load | `Notice` | new | `ui/src/blocks/Notice.tsx` | as-drawn | D3 b, ProfilesStates | 2 |
| Remove confirmation | `ConfirmRow` | new | `ui/src/blocks/ConfirmRow.tsx` | as-drawn | ProfilesStates | 2 |
| Empty profiles | `EmptyProfiles` | new | `ui/src/blocks/EmptyProfiles.tsx` | as-drawn | ProfilesStates | 2 |
| Multi-line mono text field | `TextArea` | new | `ui/src/kit/TextArea.tsx` | as-drawn | Raw | 2 |
| config.toml text editor | `RawEditor` | new | `ui/src/blocks/RawEditor.tsx` | as-drawn | Raw | 2 |
| The view | `Settings` | new | `ui/src/screens/Settings.tsx` | as-drawn | D3, Profiles, Main | 2 |

Not built: the window frame and title bar on the Settings artboards (§ 1), and the "Adjust
limits ›" line (§ 2).

## 5. Components

### Stepper (kit, PR 1)

- **What:** changes a number by fixed steps, showing it formatted. For values that apply as you
  press, where typing is not needed.
- **Placement:** `ui/src/kit/Stepper.tsx`, exported from `ui/src/kit/index.ts`, with a docstring
  (`@useWhen`, `@states`) and `Kit/Stepper.stories.tsx`; then `npm run catalog`. Built on Base
  UI's `NumberField` (Root, Group, Decrement, Input, Increment) if it can show a formatted
  string that is not a plain number ("2.0 GiB", "Off"); otherwise a group of two kit-styled
  buttons around an `<output>`, with the spinbutton contract below.
- **Fidelity:** as drawn in `D1-Allocation.png` (b): one bordered box, − on the left, the value
  centred in mono, + on the right.
- **Props:** `value: number`, `display: string` (what shows, for example "2.0 GiB"),
  `label: string` (accessible name), `step: number`, `min?: number`, `max?: number`,
  `onChange(next: number)`, `tone?: 'default' | 'success' | 'warning'` (border colour),
  `busy?: boolean` (dims the value while a change applies; presses still count),
  `size?: Size` (default `md`), `decrementLabel?`/`incrementLabel?` (default "Less", "More").
- **States:** default; hover per button (fill `surface-2`); focus-visible (the kit's
  `focusRing` on the focused part); pressed; at min or max (that button stays live, per the
  guidelines' disabled rule, and pressing it does nothing visible beyond the value staying);
  busy (`D1-Allocation.png` c, "Applying…"); tone success (`c`, "Applied"); tone warning
  (`c`, the CPU warning).
- **A11y:** `role="spinbutton"` on the value with `aria-valuenow`, `aria-valuetext` = `display`,
  `aria-valuemin`/`max`, the accessible name from `label`. ArrowUp/ArrowDown step, PageUp/Down
  step ×5, Home/End go to min/max when set. The − and + buttons are `aria-hidden` from the
  reading order only if the spinbutton itself takes focus; otherwise each is a labelled button.
- **Content:** `display` is set by the caller; longest realistic "12.5 GiB" fits the minimum
  value width.
- **Motion:** colour transitions only, the kit's default 120 ms.
- **Tokens:** border `border-border-control` (tone success `border-green`, warning
  `border-amber`); value `text-small font-medium font-mono text-fg-strong`; buttons
  `text-muted`, hover `bg-surface-2`; radius `rounded-sm`; height from `ladder[size]`.
- **Tests:** `ui/scripts/kit.test.mjs` (the kit inventory and no `className`/`style` prop);
  stories `Default`, `AtMin`, `Busy`, `Success`, `Warning`, `Focused` (`parameters.force`);
  `npm test` SSR-renders every story; a test of the step and clamp logic if it lives in a pure
  helper.

### ViewHeader (block, PR 1; `crumb` variant in PR 2)

- **What:** the header of a view pushed in place of another: back, title, an optional status
  at the right.
- **Fidelity:** as drawn in `D1-Allocation.png` (b) and, for `crumb`, `Profiles.png` (b).
- **Props:** `title`, `onBack?` (renders the back `IconButton` with label "Back"),
  `status?: ReactNode` (the strip passes "Applies now" with a green dot), `crumb?: {label,
  onClick}` (PR 2: the "Profiles ›" link before the title), `action?: ReactNode` (PR 2: Remove).
- **States:** default; back hover/focus (the kit's).
- **A11y:** the title is a heading (`h2`); back is `IconButton` "Back"; on mount the view moves
  focus to the back button so the keyboard lands in the new view.
- **Tokens:** padding `px-lg py-md`, gap `gap-sm`, divider `border-b border-border-soft`, title
  `text-body font-semibold text-fg-strong`, status `text-caption text-green`.
- **Tests:** stories `Strip`, `Crumb` (PR 2).

### MemoryBar (block, PR 1)

- **What:** how the engine's memory divides now: reserve, jobs, free budget, headroom.
- **Fidelity:** as drawn in `D1-Allocation.png` (b): title "Engine memory" and total right, one
  12 px bar of four segments with 2 px gaps, a two-column legend with a swatch per segment.
- **Props:** `totalMib`, `reserveMib`, `jobsMib`, `headroomMib` (free is derived).
- **States:** default; no jobs (the Jobs segment is absent and its legend reads "Jobs 0.0");
  over budget (free 0: the segment is absent).
- **A11y:** the bar is `aria-hidden`; the legend carries the numbers as text. The title and
  total read as one line.
- **Content:** GiB with one decimal (`(mib / 1024).toFixed(1)`), no unit on the legend values,
  "GiB" after the total.
- **Tokens:** segments reserve `bg-faint`, jobs `bg-green`, free `bg-green-soft`, headroom
  `bg-amber-soft` with a `border-l-2 border-amber`; bar height and gaps from the theme (add a
  named layout size if 12 px has no step: `--height-bar`, with its one use, per ADR-0038 § 6);
  legend `text-caption text-muted`, grid `grid-cols-2 gap-x-lg gap-y-2xs`; block padding
  `p-lg`, `gap-sm`.
- **Tests:** a pure helper `memorySegments()` in `ui/src/popover/limits.ts` with
  `ui/scripts/popover.test.mjs` cases (normal, no jobs, over budget, missing fields); stories
  `Default`, `NoJobs`, `Full`.

### LimitRow (block, PR 1)

- **What:** one live knob: label and hint on the left, the Stepper on the right, and the
  status of the last change in the hint line.
- **Fidelity:** as drawn in `D1-Allocation.png` (b, c).
- **Props:** `label`, `hint`, `stepper: StepperProps`, `status: 'idle' | 'applying' |
  'applied' | 'error' | 'warning'`, `message?` (status text), `onRetry?`, `extra?: ReactNode`
  (the *Use auto* link button).
- **States:** idle (hint in `text-muted`); applying ("Applying…", Stepper busy); applied
  ("Applied", green, for 2 s, then back to idle); error (the row takes `bg-red-soft`, hint
  "Not applied: <daemon's reason>" in `text-red`, the Stepper is replaced by `Button
  variant="secondary" size="md"` *Retry*); warning (amber hint, Stepper tone warning).
- **A11y:** the status line is `role="status"` so a change is announced; Retry is labelled
  "Retry <label>".
- **Content:** the daemon's 400 body is plain text, inserted as text, cut at 80 characters with
  an ellipsis and the full text in the title.
- **Tokens:** padding `px-lg py-sm`, divider `border-b border-border-soft`, label `text-small
  font-medium text-fg`, hint `text-caption text-muted`, gap `gap-sm`.
- **Tests:** stories per status; the debounce and status machine in a pure helper (see
  LimitsView) tested in `ui/scripts/popover.test.mjs`.

### LimitsView (screen, PR 1)

- **What:** the strip. `Popover.tsx` holds a `view: 'main' | 'limits'` state; the gauge button
  and *Limits…* set it, Back and Escape clear it. The view replaces the whole card, the faber
  header and its ⋯ menu included, as `D1-Allocation.png` (b) draws it.
- **Rows, in order:** Hard ceiling ("Most jobs at once"), Reserve ("Kept for the VM itself"),
  Headroom ("Left free between jobs"), Margin ("On learned job sizes"), Native jobs ("Without
  Docker"), then the footer *All settings…* with "⌘," at the right.
- **Applying:** each press updates the shown value at once and schedules `set_config` with that
  one knob after 400 ms of no further press on the same row (the debounce), so ten quick presses
  send one request. A response replaces the row's status; the poll that follows brings the
  snapshot in line. A press on another row does not cancel this one.
- **Hard ceiling:** § 2. Sends `hardCeiling` as a number, or `"auto"` from *Use auto*.
- **All settings…:** PR 1 sends `show_panel` (the legacy panel, which still has its Settings
  tab). PR 2 changes it to open `/settings` through a one-time session link.
- **States:** loaded; applying/applied/error per row; warning on Hard ceiling (§ 3); unavailable
  (`D1-Allocation.png` c, last card: "Limits unavailable", "faber is offline. Limits show once
  the daemon answers.", a *Reconnect* button that re-reads the snapshot); loading (before the
  first snapshot: the rows render with "…" as the value and the Steppers busy).
- **Tests:** stories `Loaded`, `Applying`, `Applied`, `Error`, `CpuWarning`, `CeilingAuto`,
  `CeilingOff`, `Unavailable`, `Dark` via the theme switch; `ui/scripts/wiring.test.mjs` gains
  the `setConfig` command; `popover-app` test that Escape returns to the main view.

### PR 2: data and shared logic

`ui/src/settings/` holds what is not drawn, with node:test cases in a new
`ui/scripts/settings.test.mjs` (registered in `ui/package.json`'s `test` glob, which already
takes `scripts/*.test.mjs`):

- `types.ts`: `SettingsView`, `FieldSpec`, `SectionSpec`, `SaveResult` as the daemon sends them
  (§ 3), plus the new `FieldSpec.narrows` and `FieldSpec.item` below.
- `api.ts`: `load()` → `GET /api/settings`; `save(config, base)` → `POST
  /api/settings?base=<digest>`. Both send the session key in `X-Faber-Session` and
  `credentials: 'same-origin'`, as the inline dashboard does. A 401 reloads the page, which then
  shows the sign-in page.
- `draft.ts`: the draft model. `dig`/`plant` by dotted key; `readValue(field, text, loaded)` ports
  `desktop/src/settings.js:179-232` exactly (blank, optional, unstated). `changes(draft, loaded,
  schema)` lists changed keys, each tagged `live` or `restart` from `FieldSpec.live`; the save bar
  reads it. `sudoCmd(template, profile)` and `narrowShown(field, value)` port `settings.js:61-80`.
- `lists.ts`: what each list may do in the form (§ 2, "Trust lists narrow in place"): `canAdd`,
  `canRemove(index)` from `FieldSpec.narrows`, and `validateItem(field, text, existing)` from
  `FieldSpec.item` (pattern, then duplicate).
- `search.ts`: `matches(query, schema)` → `[{section, field}]`, matching label, key and hint,
  case-insensitive.

### PR 2: daemon changes

- **`FieldSpec.narrows`** (`src/settings.rs`): for a `sudo` list or map, which edit narrows, so
  the form may make it: `"remove"` (Events; Environment's entries), `"remove-keep-one"` (Trusted
  actors: emptying trusts everyone), `"add"` (Denied actors, Dropped grants). Absent: the field
  stays read-only (Labels, and every other `sudo` field without one). A test holds it to
  `control.rs`: for each field with `narrows`, an edit in that direction raises no widening, and
  one in the other direction does.
- **`FieldSpec.item`**: `{pattern, example, error}` for a list that takes additions in the form:
  Denied actors (a GitHub login), Dropped grants (`ADDRESS/PREFIX PORT/PROTO`), Priority repos
  (`owner/repo`). `check-config` and the settings save validate the same pattern, so the form's
  message and the daemon's agree.
- **The form may now send `sudo` fields.** The daemon already refuses only a widening
  (`src/control.rs`), so no save path changes; the old form's round-trip-as-loaded rule becomes
  "round-trip unless edited in the `narrows` direction".
- **Raw text:** `GET /api/settings/raw` returns `{path, text, base}` (the file's bytes as UTF-8
  and the same digest `?base` checks); `POST /api/settings/raw?base=<digest>[&dry_run=1]` takes
  `{text}`, parses it into `FileConfig` (a TOML error answers 400 with `{line, column,
  message}`), then runs exactly the form's path (validation, `trust_widening`, `authorize`,
  pending restart, the live push of the five knobs) and on success writes `text` verbatim with the
  same temp-and-rename and `.bak`. It answers `SaveResult`. A `from_flags` daemon answers 409
  with a reason. Tests: a comment survives a save; a widening is refused and nothing is
  written; a TOML error names its line; a stale base is 409.
- **Routing:** `/settings` serves the React shell with the session rules of `/`; `GET`/`HEAD
  /assets/*` per ADR-0028 addendum C decision 2; the addendum gains "the shell answers `/` and
  `/settings`". The inline dashboard's Settings tab becomes a link to `/settings`.
- **Tauri:** a command that opens `/settings` through a fresh session link, as `open_dashboard`
  opens `/`. *All settings…*, ⌘, and a ⋯ menu *Settings…* call it. The panel's Settings tab
  (`desktop/src/settings.js`) is removed.

### Switch (kit, PR 2)

- **What:** an on/off setting that is neither optional nor `sudo`.
- **Placement:** `ui/src/kit/Switch.tsx`, on Base UI `Switch`, exported, with stories.
- **Fidelity:** as drawn in `Profiles.png` (b) (Git mirror on, Pull-through mirror off).
- **Props:** `checked`, `onCheckedChange`, `label` (accessible name), `size?` (default `md`).
- **States:** off, on, hover, focus-visible, pressed, changed-unsaved (the row shows it, not the
  switch).
- **A11y:** `role="switch"`, `aria-checked`; Space toggles; name from `label`.
- **Tokens:** track on `bg-green`, off `bg-border-control`; thumb `bg-surface`; `rounded-full`;
  track and thumb sized from the ladder step's icon size (a new named size if none fits, with its
  one use).
- **Tests:** `kit.test.mjs` inventory; stories `Off`, `On`, `Hover`, `Focused`, `Pressed`.

### SettingsNav (block)

- **What:** the section list beside the form, with search.
- **Fidelity:** as drawn in `D3-Window.png` (a): search at the top, then groups Resources (Limits,
  Admission tuning, Docker host), Jobs (Trust, Profiles, Runner images, Image drift, Auto-retry,
  Registry auth) and faber (Daemon, Updates); a `live` tag on Limits and Updates, the profile
  count on Profiles. The grouping maps the daemon's section titles; a section the map does not
  know goes last in faber, so a new daemon section still shows.
- **Props:** `items: {id, title, group, tag?}[]`, `selected`, `onSelect`, `query`, `onQuery`,
  `results?` (search hits as "Section › Field").
- **States:** default, item hover, selected, focus-visible, searching with hits, no hits ("No
  setting matches").
- **Responsive:** below 640 px it renders as a `Select` of sections with the search field above
  (§ 6).
- **A11y:** `nav` labelled "Settings sections"; items are links with `aria-current="page"` on the
  selected one; the search is `Input type="search"` labelled "Search settings".
- **Tokens:** `bg-surface-2`, `border-r border-border`, width a named layout size (`--width-nav`,
  180 px, its one use), items `text-small`, `rounded-sm`, selected `bg-surface font-semibold
  text-fg-strong`, group labels `text-caption font-semibold text-muted`, tags `text-caption
  text-muted`.

### SectionHeader, FieldRow (blocks)

- **SectionHeader:** the section title (`h2`, `text-body font-semibold`) and its hint
  (`text-caption text-muted`), from `SectionSpec`. As drawn in `D3-Window.png` and `Trust.png`.
- **FieldRow:** one field. Label and hint on the left, a `live` (`text-green`) or `restart`
  (`text-amber`) tag, the control on the right; `Trust.png` and `Lists.png` draw list fields in
  two columns (label column 200 px, control column the rest) and FieldRow takes `layout:
  'inline' | 'stacked'` for that. A changed field draws its control with the accent focus border
  and a `changed` tag in place of `live`/`restart`. An invalid field shows the daemon's or the
  validator's message under the control in `text-red`.
- **Control per kind:** `text` → `Input`; `int`/`float` → `Input type="number"` with the unit
  after it; `bool` → `Switch`, or `Select` of the three labels when `optional`; `enum` →
  `Select` (with the blank unstated choice when `optional`); `ceiling` → `Select` Auto / Off /
  Number plus an `Input` when Number; `csv`/`lines` → `ListField`; `map` → `MapField`; a `sudo`
  or `held` field without `narrows` → `TrustRow`.
- **Tests:** stories per kind and per state (default, changed, invalid, live, restart).

### TrustRow (block)

- **What:** a `sudo` scalar or a list the form may not edit: its value, the command that changes
  it, and the narrowing button when `narrowShown`. A `held` field (`FieldSpec.held`: Registry
  auth, whose section is held and refused on by validation) shows the same row with no command,
  since none would be accepted; its narrowing (Turn off) still shows.
- **Fidelity:** as drawn in `Profiles.png` (b) and `Trust.png` (Service public repos, On a gate
  bypass).
- **Props:** `field`, `value`, `profile?`, `pending?` (the narrowed value, unsaved), `onNarrow`,
  `onUndo`.
- **States:** at rest; narrowing pending (`bg-accent-soft`, "<old> → <new>", "Unsaved. Narrows, no
  sudo needed.", *Undo*); command copied ("Copied", 2 s, `role="status"`).
- **A11y:** the command is a `code` element; the copy `IconButton` is labelled "Copy command".
- **Content:** the command comes from `sudoCmd`, never built by string concatenation in the
  component. A command wraps onto as many lines as it needs, breaking anywhere; it is never cut
  with an ellipsis, since the operator copies it (owner, 2026-10-22). The same holds for every
  command under a ListField or MapField.

### ListField (block)

- **What:** a `csv` or `lines` field as a bordered list of items, one per row, edited per
  item.
- **Fidelity:** as drawn in `Trust.png` and `Lists.png`.
- **Props:** `field`, `items`, `loaded` (to mark added and removed items), `onChange`,
  `profile?`.
- **Modes, from `lists.ts`:** free (Priority repos: remove on every item, an add row); narrows
  `remove` (a remove `IconButton` on every item, the add command below); `remove-keep-one`
  (as remove, and removing the last item is refused with the amber note of `Trust.png`, states);
  `add` (an add row, no remove buttons, the remove command below).
- **States:** default; item removed, unsaved (`bg-accent-soft`, struck through, "Removed",
  *Undo*); item added, unsaved (`bg-accent-soft`, "Added", *Undo*); invalid entry (red border,
  `item.error` with `item.example`); duplicate ("<item> is already listed", amber); empty (the
  field's own empty line, then the add row if it has one); every item removed (amber note when
  that refuses everything, Events).
- **A11y:** the list is a `ul` labelled by the field's label; remove buttons are labelled "Remove
  <item>"; Enter in the add input adds; after a remove, focus moves to the next item's button
  (or the add input).
- **Tokens:** list `border border-border rounded-sm`, rows `min-h-control-md` via the kit's
  ladder (no raw height), `border-b border-border-soft`, items `font-mono text-small text-fg-strong`.

### MapField (block)

- **What:** a `map` field (a profile's Environment) as rows of name and value.
- **Fidelity:** as drawn in `Lists.png`: name in medium mono, value in mono, cut with an ellipsis
  and the full value in its title; remove per row when `narrows` is `remove`; the add command
  below ("edit config.toml, then `sudo faber config apply config.toml`").
- **States:** as ListField (removed with *Undo*, empty with its line), plus *Clear* kept as the
  one-click narrowing when entries remain.

### SaveBar (block)

- **What:** the floating bar that holds unsaved changes.
- **Fidelity:** raised, as drawn in `Trust.png` and `Dark.png`.
- **Props:** `changes: {key, live}[]`, `saving`, `onDiscard`, `onSave`.
- **Content:** "1 unsaved change. Applies now." / "Applies on restart." / "2 unsaved changes.
  Headroom applies now, CPU overcommit on restart." (labels from the schema, at most two named,
  then "and N more").
- **States:** hidden (no changes); shown; saving (*Save* loading, *Discard* disabled: the
  guidelines' sibling rule).
- **A11y:** `role="region"` labelled "Unsaved changes"; the text is `aria-live="polite"`.
- **Tokens:** `bg-elevate border border-border shadow-floating rounded-md px-lg py-sm`; *Discard*
  `secondary`, *Save* `primary`.

### ProfileRow, ConfirmRow, EmptyProfiles, Notice (blocks)

- **ProfileRow:** order number (`font-mono text-caption text-muted`), name (`font-semibold`),
  labels (`font-mono text-caption`), backend and Docker access at the right, chevron. As drawn
  in `Profiles.png` (a) and `ProfilesStates.png`. The native-profile state drawn there is
  gone: `backend = "native"` is a config error the save refuses (ADR-0052), so no row shows it.
- **ConfirmRow:** in-place confirmation, `bg-red-soft`: the question, what follows, *Cancel*
  (secondary) and *Remove* in a new kit Button variant `danger` (`bg-red text-bg`, which holds
  4.5:1 in both themes; `extend`, named in the guidelines' Button variants). As drawn in
  `ProfilesStates.png`.
- **EmptyProfiles:** the promo pattern of `ProfilesStates.png`: a ghost row, "A profile is what
  a job runs in", "faber can't take jobs until it has one. Add one, then restart.", *Add
  profile* (primary; outlined `secondary` while the draft is dirty).
- **Notice:** inline notices of `D3-Window.png` (b) and `ProfilesStates.png`: `tone: 'amber' |
  'red'`, title, body, optional command (`code`), actions. A full-pane "Can't load settings"
  with *Retry* when `GET /api/settings` fails.

### RawEditor (block)

- **What:** `config.toml` as text, the last nav item.
- **Fidelity:** as drawn in `Raw.png`: title, a hint, the path in mono, then a bordered editor
  with a line-number gutter (`bg-surface-2`, `text-muted`, right-aligned) and the text in mono
  12 px with 20 px lines; changed lines tinted `bg-accent-soft`, an error line `bg-red-soft`.
- **Build:** a kit-styled `textarea` (a new kit part `TextArea`, `mono`, no wrap, spellcheck
  off, tab inserts two spaces) beneath an `aria-hidden` overlay that draws the tints, with the
  gutter scrolling with it. No highlighting library.
- **Props:** `text`, `loaded` (to compute changed lines), `error?: {line, column, message}`,
  `onChange`.
- **States:** clean; changed (tints, save bar "N lines changed" plus what applies now and on
  restart, from `changes()` on the parsed dry run); TOML error (red border, the line tinted,
  "Line N: message"); value rejected (the daemon's `errors[]`); refused (the form's refused
  notice); blocked by form changes (amber notice with *Discard* and *Save* for the form's
  draft); stale (409: amber notice, *Reload*); `from_flags` (a one-line notice, no editor).
- **Flow:** typing pauses 600 ms, then a dry run (`?dry_run=1`) updates the save bar's
  live/restart text and any error; *Save* posts for real.
- **A11y:** the textarea is labelled "config.toml"; the error line is announced via
  `role="status"`; the gutter and tints are `aria-hidden`.
- **Tests:** stories per state; `settings-app` cases: a comment survives the round trip in what
  is sent; a TOML error marks its line; switching from the form with a dirty draft shows the
  blocking notice.

### Settings (screen)

- **What:** the page at `/settings`, mounted by `ui/dashboard/main.tsx` when the path is
  `/settings` (the Placeholder stays at `/`).
- **State:** the loaded `SettingsView`, the draft, the selected section (in the URL hash, so
  reload and back keep it), the search query, the selected profile, the save result.
- **Flow:** load → render the selected section → edits update the draft → SaveBar → `save` →
  on 200 reload the view and show the restart notice when `pending_restart` is non-empty; on 400
  mark each field from `errors`; on 403 show the refused notice with its sentence and command;
  on 409 reload and say the file changed elsewhere, keeping nothing (no merge).
- **Sections:** every `SectionSpec` from the schema, in the nav's grouping, plus Profiles. A
  section renders its fields in schema order; nothing is hard-coded per key except the group map.
- **Profiles:** the list (`ProfileRow`), *Add profile* appends `{name: '', labels: ['faber'],
  backend: 'docker', dind: false, ...}` and opens it; a profile opens with `ViewHeader` (`crumb`
  variant: "Profiles ›", name, *Remove*); its fields come from `profile_fields`, grouped Basics
  (name, backend), Trust (the `sudo` fields), Speed (git_mirror, mirror), Virtual machine (cpus,
  memory_mib, only for `macos-vm`).
- **States:** loading (skeleton rows), loaded, saving, saved with restart pending, invalid,
  refused, stale (409), can't load, `from_flags` (a notice that faber claims no jobs until a profile is saved), no
  profiles.
- **Tests:** stories for every state at the panel width (460) and at 1024; a
  `ui/scripts/settings-app.test.mjs` in the `popover-app` style (jsdom + stubbed `fetch`):
  load, edit, save, 400 marks fields, 403 shows the refused notice, removing a trusted actor
  sends the narrowed list, removing the last one is refused in the form.

## 6. Composition

### The strip

The whole popover card, in place of its header and body: `ViewHeader`, `MemoryBar`, five
`LimitRow`s, the footer. The card's width is fixed; the window grows to fit (≈ 420 px), under
`CARD_MAX`. The footer uses `bg-surface-2`, like the popover's other toolbar.

### The Settings view

A two-column page: `SettingsNav` (fixed 180 px, `bg-surface-2`, `border-r`) and the content
column (`flex-1`, `px-3xl`, max line length from the theme). Each section is `SectionHeader`
then its rows; only the selected section shows. The `SaveBar` floats `bottom-lg` inside the
content column and the column pads its bottom by the bar's height so the last row is never
hidden. Below 640 px wide the sidebar becomes a `Select` of sections above the content, and the
search field moves with it.

## 7. Out of scope

- VM sizing (`[linux_vm]`, the machine share): a separate session.
- The rest of the React dashboard (Needs attention, Now, Capacity, History) and moving `/` to
  React; the panel window becoming the dashboard (addendum C decision 1).
- Per-class pins (`/api/pin`): they stay in the legacy panel (its Pin button pins a class at its
  current caps). The inline dashboard shows the caps read-only.
- Larger text sizes, and the restart flow after a save (drain, then come back).
