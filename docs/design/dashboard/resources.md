# Dashboard: Resources view — anatomy

Reference: `./reference/R-Resources-light.png`, `./reference/R-Resources-dark.png` (frozen
2026-10-30; no longer in this tree, since no story renders the unbuilt view, and the originals
stay with the legacy repository), three artboards: the busy macOS host with one stale
measurement and one failure, which drew Disk, VMs, Services, Mirrors and Caches in that order,
each a heading with a one-line state over a table; a fresh host before the first measurement,
with each section that is off as one line naming the setting that turns it on; and a Linux
host, whose VMs section is one line saying Linux hosts run Docker jobs on the host. A change
to this view updates these images in the same change · Decided:
[ADR-0054](../../adrs/0054-the-dashboard-is-views-in-a-sidebar.md) amended 2026-10-30 (a
Resources view; git mirrors move to it), layout D4, owner 2026-10-30. Components and shell:
[`anatomy.md`](anatomy.md), [`screens.md`](screens.md).

Status: **anatomy, decided, unbuilt.** No open question. Delivery is R7: step 1 the daemon
fields (§ 5), step 2 the view (§ 4).

## 1. The screen

- **Purpose:** answer "what does faber keep running or keep on disk for my jobs, is each
  thing on, and what does it cost me". Today the operator learns that a VM, a Postgres
  server, a mirror or a cache exists only when it raises an alarm, and no screen says how
  much disk any of them takes.
- **Who:** the operator of one faber host, in the app's window or a browser (`faber
  dashboard`), as for every view (`screens.md` § 1).
- **Entry points:** `/resources`, a sidebar item between Memory and Repositories
  (ADR-0054 § 1). Overview's Host panel *Disk* row links here (§ 4). The popover does not
  change: it already shows the VMs' rows, and it does not gain the rest.
- **Data:** `GET /api/state`, polled every 2 s like every view. Memory and state are live.
  Disk sizes are the janitor's last measurement, served with their age (D8, R1), because
  listing volumes costs a Docker call and an image store costs a walk (§ 5).
- **Primary viewport:** 1280 px. At `narrow` (640 px) the sections stack and each table
  turns into a list of rows (DataTable's narrow form).
- **Sparsest real data:** a fresh macOS host with only the Linux VM: one VM row, and
  Services, Mirrors and Caches each one *Off* line. On Linux there are no VMs, so that
  section is one line too: "Linux hosts run Docker jobs on the host; faber runs no VMs
  here." Every section needs a sentence for its empty and its off state.

## 2. Decisions

**Decided (owner, 2026-10-30):**

- **D1: one view for resources, not a panel in Memory.** Memory is the memory budget.
  What this view is mostly about is disk, which no view shows, and things that are on or
  off, which Memory has no place for.
- **D2: not in the popover.** The popover stays an attention-first glance
  (`docs/design/popover.md`). Its VM rows stay as they are.
- **D3: an *Off* kind is shown, never hidden.** "Off" and "on with nothing running" are
  different answers, and the second one is the one people miss.
- **D4: the layout is "Disk rail over kind tables"** (owner, 2026-10-30), picked from three
  directions and a hybrid: Ledger (one disk-first table), Kind panels (summary tiles and a
  2×2 grid of panels) and Where it lives (one containment tree). The view opens with a Disk
  panel (total, rail, and the engine disk's own split as a second rail under it), then one
  full-width panel per kind, each with On or Off in its header and a table shaped for that
  kind. Ledger was rejected because one table forces VMs and caches into the same columns;
  Kind panels because nothing says where the disk went; Where it lives because VMs and
  services that hold no disk have no place in a disk tree, and it needed a new component.

**Taken in this doc, from ADR-0054 (no new decision):**

- **Read-only.** No button on this view. An action on a resource (verify a golden image
  now; any future "trim now" or "rebuild mirror") lives in Maintenance (ADR-0054 § 2).
- **Alarms stay on Overview** (ADR-0054 § 3). A mirror that is down or a blocked git
  mirror already raises an alarm. This view shows the same fact as a row's state, and links
  to nothing.
- **One full form** (ADR-0054 § 4). A repository's cache and git mirror are shown in full
  here. Repositories (#421) may summarise them per repository and link here.

**Decided in the grilling (owner, 2026-10-30).** D5 to D8 were each put to the owner and
agreed; R1 to R8 the owner delegated ("fill in with recommendations"), so they are the
recommendations as written here and stand until the owner changes one.

- **D5: two disks, two rails** (Q1). On macOS the Disk panel draws the Mac volume that
  holds the VMs (golden images, the other image-store files, and the engine disk file) and,
  under it, the Linux engine disk (images and job containers, caches, the mirror store, git
  mirrors, Postgres data). Each rail carries its own free space and its own floor, in the
  words of what it stops: "macOS VM jobs stop below 10 GiB" on the Mac volume
  (`host_free_mib`), "Docker jobs stop below 10 GiB" on the engine disk (`vm_free_mib`,
  #410). A Linux host has one rail, the host disk. *Why:* the disk gates read two
  disks; one rail put the Docker floor next to the wrong number.
- **D9: the engine disk's size is read, not assumed** (ADR-0061 §6). Its rail's total is
  `budget.disk.vm_grow.disk_bytes`, the file's length as the VM supervisor reports it, and
  the ceiling beside it is `max_bytes` (`[linux_vm] disk_max_gib`); "of 64 GiB" only while
  that is the size. A pending grow (`pending_to_bytes`) and what it waits for
  (`waiting_for`) are one line under the rail; a short disk that cannot grow says why in
  the same place, and a standing `fault` reads amber. `vm_grow` is null on a host with no
  grow loop (Linux, LEGACY): the rail then shows no total. *Why:* the disk grows within a
  ceiling now, so a fixed figure would be wrong after the first grow.
- **D6: the engine disk at its real size on the Mac** (Q2). Its segment on the Mac rail is
  the file's allocated size, and when that exceeds the space in use inside the VM by more
  than 1 GiB a line says so: "20 GiB freed inside the VM, not returned to the Mac". *Why:*
  whether freed space comes back is unmeasured (launcher README check 9); the gap is how
  the operator sees it, and the #423 class grows there unseen. If `_faber` cannot read
  the file's allocated size, the segment uses the VM's own reading, marked "estimated".
- **D7: the sidebar count is the tighter disk's free space** (Q3, was O1): "25 GiB free",
  from whichever rail is closer to its floor, in neutral colour. *Why:* it says when to
  open the view, it reads what the gates read, and the alarm stays on Overview
  (ADR-0054 § 3). A total of bytes held was rejected: with two disks it means nothing
  single, and it never says whether anything is wrong.
- **D8: measure with one Docker call, scan only where a trim runs** (Q4, was O2). Every
  janitor tick (900 s) faber runs one `docker system df -v` against the engine, under a
  timeout, for every volume's size: caches, git mirrors, the mirror store and service
  data. The per-volume scan (`src/cache.rs` § 3) still runs only where a limit makes trim
  run, and adds entries, the oldest entry's age and the last trim. With no limit set, a
  cache row shows its size and "no limit set" where the entries would be. *Why:* a
  container per volume per tick is wasted work for sizes Docker already knows, and
  hiding the unlimited cache hides the one most likely to grow.
- **R1: measurements age visibly.** Sizes carry their age in the panel footer ("measured 6
  min ago"); after two ticks (30 min) the age turns amber with "stale". A failed
  measurement keeps the last good numbers and puts the error in the footer, in mono. A size
  never measured reads "not measured yet", never 0.
- **R2: golden images are named on the Mac rail only.** Each golden image is a legend entry
  with its size. Its standing and *Verify now* stay in Maintenance's Golden images list
  (ADR-0054 § 4); the VMs panel's footer links there instead of repeating them.
- **R3: per-job services are counted, not listed.** One row per service table with its
  image, its memory per job and how many run now. The job each serves is in Activity and
  the popover.
- **R4: the pull-through mirror's per-repository counters** open from the store row
  (RowDisclosure): every repository with hits, misses, bytes from cache and bytes from
  upstream, sorted by bytes from upstream, the number that costs time.
- **R5: Overview's Host panel *Disk* row is unchanged** (it already reads the Docker disk,
  `budget.disk.free_mib`) and gains the "Where it goes" link here.
- **R6: at `narrow` (640 px)** the two rails stack full width, legends wrap, and each table
  uses DataTable's narrow form. Nothing scrolls sideways.
- **R7: three changes, in order.** This docs change (ADR-0054 amendment, this anatomy, the
  references); then the daemon fields of § 5 with the UI types, additive and inert; then the
  view, its stories, the nav item and the Overview link, with a design review (AGENTS.md
  § Design).
- **R8: a Linux host's one rail** is the host disk: Docker's images and containers, then
  the same volume breakdown as the engine disk on macOS, from the same `docker system df
  -v`.
- **O3 → a stacked rail** (D4): one accent with gaps (`anatomy.md` § 2 O4), its legend under
  it as text for screen readers and narrow widths. See § 6 for what may and may not be
  summed.
- **O4 → VMs, Services, Mirrors, Caches** (D4): running things before stored things.

## 3. Code archaeology

What `/api/state` already serves and the dashboard drops:

- **VMs:** `budget.vm.vms[]` (`VmEntry`, `ui/src/popover/types.ts`): kind, slot, state,
  stop reason, started, CPUs, memory, and the job it serves. Plus `macos_cap`,
  `macos_running`, `linux_vm_mib`, `macos_vm_mib`, `images`, `golden[]` and
  `listed_age_s`. Only the popover renders them (`ui/src/popover/vms.ts`).
- **Git mirrors:** `git_mirrors[]` (`git_mirror::MirrorView`, ADR-0035 § 5): repo, ready,
  refreshing, `size_kib`, last refresh, last attempt, last error, blocked. Only
  `dashboard/alarms.ts` reads it (a blocked mirror or a failed refresh).
- **Pull-through mirror:** `mirror` (`mirror::MirrorView`, ADR-0047), `null` with
  `[mirror]` absent: `state` (`ok`, `passing_through`, `starting`, `faltering`, `down`,
  `image_missing`), `max_size_gib`, `image`, `version`, `last_healthy_ms`, `attached`,
  `attention[]`, and `lfs` and `npm` (each: `fill`, objects, bytes, `max_bytes`, and per-repo
  hits, misses and bytes). The UI's `MirrorView` type lacks `image`, `version`,
  `last_healthy_ms` and `attached`; only `attention` is rendered.
- **Disk gates:** `budget.disk`: mode, floor, `free_mib` (the Docker disk: the engine disk
  when it can be read, else the host volume) with `volume` and its age, plus
  `host_free_mib`, `vm_free_mib`, `vm_reading_age_s` and `vm_unreadable` (#410,
  `DiskLedger` in `src/limiter.rs`). The Mac volume gates macOS VM jobs and reviving the
  engine; the engine disk gates Docker jobs. Overview's Host panel shows `free_mib`.

What the daemon knows and does not serve:

- **Services** (`src/services.rs`, ADR-0046): the policy (`ServicesPolicy`: Postgres
  versions, image, memory, `idle_stop`; each per-job table's image and memory) and, per
  Postgres major version, a `Slot`: running or not, the memory it holds, the runners
  attached, idle since. Per-job service containers (`faber-svc-<name>-<id>`) live and die
  with their runner. Each shared server has a data volume. Only `service_refused[]`
  reaches the state.
- **Build caches** (`src/cache.rs`, ADR-0005, ADR-0055): opt-in per profile
  (`cache_volume = true`), then one volume per repository, labelled
  `org.faber.kind=cache`. A trim scans each volume read-only and knows its size, its
  entries and their ages (`Scan`), then discards it. The limits are `[host]
  cache_max_gib` and `cache_max_age_days`; with neither set faber trims only under disk
  pressure.
- **VM image store** (`src/setup/vm_images.rs`): golden images in `IMAGE_STORE`, the Linux
  base image (`IMAGE_STORE/linux/disk.raw`), runner tarballs (`runner/`) and tools disks
  (`tools-disks/`). Nothing measures them. The engine VM's own disk, in its bundle under
  `<vm_home>/linux/`, is a separate sparse file of a fixed 64 GiB (`DISK_BYTES`,
  `src/setup/linux_image.rs`) and the one that fills (#423). Whether space freed inside it
  returns to the Mac is unmeasured (launcher README check 9).
- **Whether a kind is on:** `git_mirror`, `mirror` and `cache_volume` are per-profile
  keys, and `[mirror]`, `[mirror.lfs]`, `[mirror.npm]` and `[services.*]` are host tables.
  `ProfileView` carries none of the profile keys, so the UI cannot tell "off" from "on,
  nothing yet".

Where things live, which decides what may be added up (§ 6): on macOS every Docker volume
(caches, git mirrors, the mirror's store, service data) is inside the engine VM's disk, and
macOS VM slots are APFS clones of a golden image that share its blocks. On Linux, Docker
volumes are on the host's disk and there are no VMs.

## 4. Inventory

Each section is a Panel with a title, a one-line state in its header (On, Off, or a count),
and a body. An *Off* body is one sentence naming the setting in mono and linking to its doc
(`docs/mirrors.md`, `docs/git-mirror.md`, ADR-0046, ADR-0055). None of these keys is
editable in Settings as of this writing, so the line names the key; it links to a form
only once Settings can edit it. Sizes age as R1 says.

| Place | State / content | Off / empty |
|---|---|---|
| Disk (top, D5) | macOS: the Mac volume rail (golden images by name, the other image-store files, the engine disk file at its allocated size, free) with "macOS VM jobs stop below N GiB", the D6 gap line when it applies, then the engine disk rail (images and job containers, caches, mirror store, git mirrors, Postgres data, free, "of 64 GiB, grows to 128 GiB" from `budget.disk.vm_grow`: its real size and its ceiling, never a fixed 64, D9) with "Docker jobs stop below N GiB", and under it, while a grow is pending, "Growing to 96 GiB · waiting for an idle moment" (or why a short disk does not grow, or the `vm-disk-grow` fault, amber). Linux: one host-disk rail (R8) | Never off. Before the first measurement: free space on each disk only, and "sizes after the first sweep, within 15 min". Engine disk unreadable: its rail shows `vm_unreadable` in mono and the Mac rail carries the Docker floor, as the gate does |
| VMs | One row per `budget.vm.vms[]`: kind and slot, state in words (`popover/vms.ts`' vocabulary), CPUs, memory, uptime, the job it serves (repo and job, or "a fork's job" when `job_name` is null), stop reason when stopped. Header: "2 running · macOS 1 of 2". Footer: "Golden images, their standing and Verify now: Maintenance" (R2) | Linux host: the sentence in § 1. macOS with no `macos-vm` profile: the Linux VM row only, and "macOS VM jobs: off (no `macos-vm` profile)" |
| Services | One row per offered service. Postgres: one row per allowed major version, "Running, 3 jobs", "Idle, stops in 4 min" (from `idle_stop`) or "Stopped", memory held while running, its data volume's size. A per-job service: its image, memory added per job, and how many run now (R3). Refused jobs stay in Activity; the header links there when `service_refused` is not empty | "Off: no `[services.*]` table" |
| Mirrors · git | One row per `git_mirrors[]`: repo, Ready / Refreshing / Blocked, size, last refresh; the last error in mono under the row when set. Header: which profiles have `git_mirror = true` | "Off: no profile sets `git_mirror = true`" |
| Mirrors · pull-through | State in words from `mirror.state`, version, jobs attached, last healthy. One row per ecosystem (LFS, npm): fill (`relaxed` or off), objects, bytes against the store's one cap (Meter), hit rate. A RowDisclosure opens the per-repo counters (R4). Header: profiles with `mirror = true` | "Off: no `[mirror]` table" (`mirror` is null). LFS or npm alone off: its row says which table turns it on |
| Caches | One row per cache volume: repository, size (Meter against `cache_max_gib` when set), and where a trim runs (D8) entries, the oldest entry's age, last trim and what it deleted; else "no limit set". Header: the profiles with `cache_volume = true`, and the policy in words: "Up to 20 GiB and 14 days per repository", or "No limit; faber trims only when the disk is low" | "Off: no profile sets `cache_volume = true`". On, empty: "No repository has a cache yet." |

Overview gains no section (ADR-0054 § 4). Its Host panel's *Disk* row gets a link,
"Where it goes", to this view.

## 5. Daemon additions (step 1)

All additive: an older dashboard ignores them, and a missing field means "not known", as for
`VmEntry`. Names are provisional until the step's PR.

- **`services`** (`null` with no `[services.*]`): `postgres` (`null` unless configured):
  `idle_stop_s`, `memory_mib`, and `servers[]`, one per allowed version: `version`,
  `state` (`running`, `starting`, `stopped`), `users` (a count, never claim ids),
  `idle_since_ms`. Its data volume's size comes from `volumes`. `per_job[]`: `name`,
  `image`, `memory_mib`, `running` (a count).
- **`volumes`** (D8): `measured_ms`, `error` (the last failed measurement's text, or null),
  `images_bytes` and `containers_bytes` (Docker's own totals), and `items[]`: `name`,
  `kind` (from `org.faber.kind`: `cache`, `git-mirror`, `mirror`, `service`), `repo` or
  `service` where the label names one, `bytes`. From one `docker system df -v` per janitor
  tick, under a timeout; the last good reading is kept with its age.
- **`caches`**: `limits` (`max_gib`, `max_age_days`, and `policy`: `operator` or `faber`)
  and `volumes[]`: `repo`, then, only where a trim ran, `entries`, `oldest_age_s`,
  `last_trim_ms`, `last_trim` (what it deleted, in words). Sizes come from `volumes`.
- **`budget.vm.store`** (macOS only): `measured_ms`, `golden[]` (`image`, `bytes`),
  `linux_base_bytes`, `runner_bytes`, `tools_bytes`, and `engine_disk`: `allocated_bytes`
  (the file's size on the Mac, D6; null when `_faber` cannot read it) and `max_bytes`.
- **`budget.disk.vm_grow`** (ADR-0061, D9): `disk_bytes` and `disk_allocated_bytes` (the
  engine disk file's length and what it holds on the Mac, from the VM supervisor),
  `max_bytes` (the ceiling), `pending_to_bytes`, `waiting_for` and `fault`. Null where no
  grow loop runs.
  Used space inside comes from `budget.disk.vm_free_mib`. Measured on the janitor's tick.
- **`ProfileView`** gains `git_mirror`, `mirror` and `cache_volume` (booleans), so a
  section can say which profiles use it and tell off from empty.
- **`mirror`**: no change; the UI type catches up with what the daemon sends (§ 3).
- **`git_mirrors[]`**: no change.

Privacy: the view shows repository names and job names the state already carries, and
nothing more. No data volume's contents, no credential, no service's environment (a
per-job table's `env` travels name-only already, and the view does not show it).

## 6. Composition and rules

- **Never add up bytes that sit inside each other.** On macOS, caches, mirrors and service
  data are inside the engine VM's disk: the Mac rail shows the engine disk as one segment
  and the engine rail breaks it down beneath, never beside. A macOS VM slot's clone is not
  counted (it shares the golden image's blocks); golden images are counted once. On Linux
  the volumes sit beside each other on the host disk.
- **Bytes are binary units**, one formatter shared with Memory (GiB, MiB); a size under
  1 MiB reads "< 1 MiB".
- **State words, not codes**, as in Activity (S11): a code the view does not know shows as
  itself.
- **Calm by default** (S16): a healthy running thing is normal text with a quiet dot. Amber
  marks stale measurements, an idle-but-holding server and a cache over its limit before
  the next trim; red only what an alarm also says (a mirror down, a blocked git mirror).
- **No row opens a detail pane** except the mirror's per-repo counters. A VM's job is named,
  not linked: `VmEntry` carries no run id.

## 7. Components

Reused from the kit and blocks, none new; the rail generalises BudgetRail:

- **Panel**, **DataTable**, **KeyValueList**, **Meter**, **StatusDot**, **RowDisclosure**
  (`anatomy.md` § 5).
- **BudgetRail (extend):** today it draws the memory budget's segments. Resources needs the
  same rail over bytes, with a nested breakdown (§ 6). Extend it to take generic segments
  and a unit, so the two rails look like one system.
- **DashboardNav (extend):** one more item; its count follows D7.

Stories: one per section in each of off, empty, healthy, busy and a failure, plus a sparse
macOS host, a Linux host and a stale measurement, with visual baselines in light and dark,
as for every view.

## 8. Out of scope

- Any action on a resource: start, stop, trim, rebuild. They go in Maintenance, each in its
  own change.
- Editing the keys that turn a kind on. That is Settings' job; the Off line names the key.
- History of sizes over time (would ride on #418's persistent history).
- VMs that are not faber's, such as Docker Desktop's (#375).
- Per-repository views of the same data (#421).
