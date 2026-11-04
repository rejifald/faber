---
title: The dashboard is views in a sidebar
type: adr
status: accepted
date: 2026-10-28
owner: rejifald
---

# ADR-0054: The dashboard is views in a sidebar

## Status

accepted (2026-10-28). Supersedes decision 4 of
[ADR-0041](0041-the-dashboard-is-the-one-full-ui.md) (one page in four groups). ADR-0041's
other decisions stand: the dashboard is faber's one full UI, the panel window loads it, the
panel's controls move, and no section exists in two places (restated below for views).
Amended 2026-10-30 (owner): a Resources view joins, and git mirrors move to it from
Overview (decision 2).

## Context

ADR-0041 grouped the dashboard into one long page (Needs attention, Now, Capacity,
History) with a sidebar that jumps between the groups. It rejected tabs per group because
capacity and history would hide behind a click, and ⌘F could not reach them.

The direction-B redesign (`docs/design/dashboard/anatomy.md`, frozen references in
`docs/design/dashboard/reference/`) was drawn on a canvas against today's page and two
other directions, and the owner picked its sidebar of five nouns, N2: Overview, Activity,
Memory, Repositories and Settings. Three things moved since ADR-0041:

- **History outgrew a group.** Recent runs need filters by outcome (delivered, faults,
  refused, retried), a row that opens to its detail, and more than the last few rows. On
  one page that table either stays short or pushes everything under it out of reach.
- **Repositories is a new subject.** Which repositories faber serves, each one's needs,
  holds and recent outcomes, and later the repositories it finds by their workflow files.
  It has no home in the four groups, and ADR-0041 makes a new group a change to that ADR
  anyway.
- **The sidebar can carry the signal a click would hide.** Each destination shows a count
  ("Activity, 7 faults", "Memory, 62 %"), so a view's state is visible without opening it.
  That answers most of why tabs were rejected.

The components are built (`ui/src/kit/`, `ui/src/blocks/`, PR #413). `DashboardNav` works
both as page links and as section anchors, so either structure could be wired. This ADR
picks one.

## Decision

1. **The dashboard is seven views, one at a time, chosen from the sidebar:** Overview,
   Activity, Memory, Resources, Repositories, Maintenance, Settings. Each is its own URL.
2. **What each view holds:**
   - **Overview** (`/`): everything that needs a decision now, plus the current state.
     That means ADR-0041's whole Needs attention group (notices, refused, bypasses,
     warnings, faults, credential, holds, updates) and its Now group (counters, in-flight
     runners, auto-retries), plus host health (Docker,
     disk, memory pressure, uptime). It also carries a summary of each other view, as drawn
     in `docs/design/dashboard/reference/B-Overview.png`: the memory budget rail and the
     learned caps (from Memory), the latest runs with their outcome filter (from Activity),
     and four 24-hour stat tiles (delivered, faber's own faults, wait to admit, refused at
     the gate). When nothing needs attention, that part is one quiet line, as before.
   - **Activity** (`/activity`): recent runs and recent decisions, filtered by outcome,
     each row opening to its detail. This is ADR-0041's History.
   - **Memory** (`/memory`): the memory budget in full, what parked jobs, and the
     per-class caps (learned and pinned). This is ADR-0041's Capacity.
   - **Resources** (`/resources`): what faber keeps running or keeps on disk for jobs,
     beyond the jobs themselves: the VMs it runs, the services it provides (the shared
     Postgres servers, per-job service containers), the mirrors (git, and the pull-through
     mirror's LFS and npm stores) and the per-repo build caches. Each kind says whether it
     is on, what it is doing now, the memory it holds and the disk it takes against its
     limit. A kind that is off is one line naming the setting that turns it on, never
     absent, so "not set up" and "nothing running" read differently. The view is read-only:
     an action on a resource lives in Maintenance, and its alarm on Overview. Its content is
     in `docs/design/dashboard/resources.md`.
   - **Repositories** (`/repositories`): one row per repository faber serves, with its
     needs, holds and recent outcomes. Its content is designed in its own step (canvas
     directions R1 and R2) and needs a daemon endpoint. Until then the view is absent from
     the sidebar, not shown empty.
   - **Maintenance** (`/maintenance`): every action that changes what the host does, each
     beside the state it changes: Pause or Resume, Reconcile now, Hold Docker admission or
     Release it, clear a repository's hold, verify a golden image, check, install or skip an
     update, and Drain, the one that stands the host down until faber restarts, behind a
     confirmation. Overview's header carries none of them (owner, 2026-10-28): an alarm card
     still offers the one fix it names (Release, Clear, Install), so no decision hides
     behind a click. Pinning a class stays with the class, on Memory.
   - **Settings** (`/settings`): unchanged.

   Peers live in the sidebar's host card, on every view. Git mirrors live in Resources
   with the other mirrors (amended 2026-10-30; they were to go with the runners they serve,
   in Overview's Now part, which was never built).
3. **Nothing that needs a decision hides behind a click.** Overview carries every
   attention item, whatever view its detail lives in, and the sidebar shows each view's
   count. A view may be the only place a fact's *detail* lives, but never the only place
   its *alarm* shows.
4. **No section exists in two places** (ADR-0041 decision 5, restated). A section's full
   form lives in exactly one view. Overview may show a summary of another view's section
   (the budget rail, the learned caps, the latest runs, a stat tile), and that summary links
   to the view, where the full form is: every run, every class, every filter. A new section goes
   into one view; a new view is a change to this ADR.
5. **Views are real URLs.** Each is a flat path the daemon serves with the same React
   shell, behind the session like `/` (ADR-0028 addendum C). Paths are one segment and
   never take a trailing slash, because the shell's asset URLs are relative (`src/web.rs`,
   `settings_shell`). Back and forward move between views, a view can be bookmarked or
   linked from a notice, and the popover's links can open a view directly. Filters inside
   a view (Activity's outcome) live in the query string.
6. **Below the narrow breakpoint** the sidebar becomes a Select of the same views, as
   `DashboardNav` already does.

## Consequences

- ⌘F reaches only the open view. This is the cost ADR-0041 named, and it is accepted
  because no alarm depends on it (decision 3). Activity's filters replace searching history
  with ⌘F.
- The daemon gains a shell route per view. The vanilla `INDEX_HTML` at `/` retires once
  Overview replaces it, along with its 13 sections and roughly 20 notice slots.
- Overview's stat tiles need history the daemon does not serve yet: a 24-hour series for
  each sparkline, the last hour's fault count, p95 wait to admit. Until it does, they show
  the number without the trend.
- Repositories needs a daemon endpoint. `StateView` is built around events, not
  repositories.
- Resources needs `StateView` to serve what it does not yet: the services faber runs, the
  build caches' sizes and the VM image store's size. Disk sizes come from the janitor's last
  measurement, with its age, never from a scan per poll. A repository's own cache and git
  mirror are Resources' full form; Repositories may summarise them and link here
  (decision 4).
- The panel-window plan (ADR-0041 decisions 1 to 3) is unchanged. The window opens
  Overview.
- `docs/design/dashboard/anatomy.md` § 2 O1 ("build neutral") is settled. The shell wires
  `DashboardNav` in `page` mode.

## Considered and not adopted

- **One page in four groups (ADR-0041 decision 4):** keeps ⌘F across everything, but
  Activity's filters and row detail, and Repositories, do not fit as groups on one page.
  The page grows back into the long stack ADR-0041 set out to fix.
- **One page with a separate History view:** a hybrid. It fixes Activity, but leaves
  Memory's detail on the page and gives the sidebar two kinds of link, a jump and a view,
  that look the same.
- **Tabs inside one view:** no URL per view, and no room for the counts that make the
  sidebar answer the ⌘F objection.
