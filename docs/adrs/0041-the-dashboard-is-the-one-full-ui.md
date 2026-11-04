---
title: The dashboard is the one full UI
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0041: The dashboard is the one full UI

## Status

accepted (2026-10-18), as a design decision. How the app window loads the dashboard
(the session, the window's privileges) needs its own ADR, an addendum to
[ADR-0028](0028-the-control-api-knows-its-caller.md), before anything is built. That addendum
is [ADR-0028 addendum C](0028-the-control-api-knows-its-caller.md#addendum-c-2026-10-18-the-panel-window-and-the-dashboards-assets)
(2026-10-18).

Decision 4 (one page in four groups) is superseded by
[ADR-0054](0054-the-dashboard-is-views-in-a-sidebar.md) (2026-10-28): the dashboard is five
views in a sidebar. Decisions 1, 2, 3 and 5 stand.

## Context

faber has two full UIs that show the same thing:

- **The panel** (`desktop/src/index.html`, `main.js`, `settings.js`, `styles.css`): the
  app's window, reached from the popover's ⋯ menu and ⌘⇧F. Its tabs are Status and
  Settings.
- **The web dashboard** (`src/web.rs`), served by the daemon on loopback behind a session
  (ADR-0028). Its tabs are Overview and Settings.

Both carry runners, per-class caps, faults, recent runs, recent decisions and profiles,
and both have a settings form. The dashboard has about seven sections the panel lacks
(memory budget, gates, refused, dind refused, bypasses, warnings, retries). Every section
is built twice, in two stylesheets and two render scripts. That is where the drift
measured in [`docs/design/drift-2026-10-18.md`](../design/drift-2026-10-18.md) came from.
Linux hosts have no app, so the dashboard has to be complete regardless.

The dashboard itself stacks about 20 notice slots above its tabs, then 13 sections in a
fixed order. That is the one-slot problem the popover's redesign fixed
([`docs/design/popover.md`](../design/popover.md)), turned into a long stack.

Three structures were rendered on live state: today's, a grouped page with a sidebar, and
one tab per group.

## Decision

1. **The dashboard is faber's one full UI.** The popover stays the native glance surface
   (`popover.md`). The panel stops being a separate UI: its window shows the dashboard.
2. **The panel window loads the dashboard** with a session the app mints for it, the way
   *Open dashboard* already does. The page gets **no Tauri IPC**: it is a web page in an
   app window, with the same rights as in a browser. The mechanism is decided in the
   ADR-0028 addendum.
3. **The panel's own controls move:** installing or skipping an update, and start at login,
   go to the popover's ⋯ menu or into the dashboard. Nothing the panel can do today is
   lost.
4. **The dashboard is one page in four groups,** with a sidebar that jumps between them,
   and Settings as its own view:
   - **Needs attention:** every notice and every ⚠ section (refused, bypasses, warnings,
     faults, credential, holds, updates). It shows only what is active. When nothing is,
     the group is one quiet line.
   - **Now:** the actions (Reconcile, Pause, Hold Docker), the counters, in-flight runners,
     auto-retries, peers and git mirrors.
   - **Capacity:** the memory budget, what parked jobs, per-class caps.
   - **History:** recent runs and recent decisions.

   Everything stays on one page, so ⌘F finds it.
5. **No section exists in two places.** A new section goes into one of the groups, and a
   new group is a change to this ADR.

## Consequences

- `desktop/src/index.html`, `main.js`, `settings.js` and `styles.css` are retired once
  the window shows the dashboard, roughly 1,950 lines.
- The design system's builds (ADR-0037 to ADR-0040) still apply to the panel's files until
  then. The work there is short-lived, so it should stay minimal.
- The panel window needs the daemon. When the daemon is down it shows the offline state,
  as the panel's offline section does today.
- Settings has one form, not two.

## Considered and not adopted

- **A curated panel beside a full dashboard:** this halves the duplication, but the
  sections both keep are still built twice.
- **Keeping both full UIs:** the cause of the drift.
- **Tabs per group:** shorter views, but capacity and history stay hidden until clicked,
  and ⌘F cannot reach them.
