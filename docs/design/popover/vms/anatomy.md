# Tray popover, Virtual machines section: anatomy

Reference: [`./reference/`](reference/) (frozen 2026-10-29; every artboard in `-light` and
`-dark`) · Decided: 2026-10-29 by the owner, direction A with C's total · Content canon:
[`../../popover.md`](../../popover.md) § Virtual machines

Re-rendered on 2026-11-03 from the Storybook stories, with illustrative data; the frozen originals
stay with the legacy repository.

The artboards of the three directions compared and of the caption they replaced (`a-vm-rows-*`,
`b-gate-shares-*`, `c-vm-table-*` and `current-*`) are no longer in this tree, since no story
renders them; the originals stay with the legacy repository. `a-vm-rows-*` drew direction A, a row
per VM under the gates, each with a spec line; `b-gate-shares-*` drew B, each VM's share as a
segment in the memory, CPU and disk bars, with a legend under each; `c-vm-table-*` drew C, a table
of the VMs with vCPU, memory and disk columns and a total row; and `current-*` drew the popover as
it was, with the one caption under "Other hosts".

The popover listed VMs as one faint caption, `VMs: linux #0 in service | macos #1 booting`,
under "Other hosts". It showed no resources, and it was hidden whenever the popover was idle.
This section replaces that caption with a row per running VM: what the VM was given, and what
it is running.

## 1. The screen

- **Purpose.** It answers "which VMs are up, what does each hold, what is each doing?" at a
  glance, beside the gates whose memory and CPU those VMs take.
- **Who.** The operator, opening the popover from the menu bar.
- **Data.** `budget.vm.vms[]` in `/api/state`. Each row needs kind, slot, state, vCPUs,
  memory, start time, and the job it serves (§ 5, *Data contract*). The Linux VM's free disk
  is `budget.disk.vm_free_mib` (already sent, #410). Its job count is
  `budget.docker_inflight` (already sent).
- **Viewport.** The popover's two fixed widths (guidelines § Layouts): the 240 px left column of
  the 620 px card while job groups show, and the 308 px single column otherwise.
- **Sparsest real data.** One Linux VM, up and serving nothing (reference `rec-idle-*`). Then
  no VM at all: the section is absent (`rec-no-vms-*`).

## 2. Decisions

- **A row per VM** (direction A), in the left column under the gates and above Other hosts.
  Why: it names each VM, what it holds and what it runs. The gates stay the admission ledger.
- ~~**The total sits on the section label**~~ (from direction C). *Superseded 2026-10-31
  (owner):* the label carries no total; each row opens to say what its VM was given.
- **Rows open in place, like the gates** (owner, 2026-10-31). Shut: a status icon, the kind,
  what it serves, and the age. Open: State (settling, stopping, unmanaged), CPU, memory and
  (Linux) free disk as term/value lines (`FactList`, shared with `GateRow`). Was: an always-shown mono spec line under each row.
- **The label carries no state and no slot** (owner, 2026-10-31). Shut, a row reads
  `Linux | 3 jobs` or `macOS | <job name>`: only what it serves. `booting`, `stopping` and
  the rest are the icon's, and the open row spells them out under `State` (one hourglass
  covers booting and proving the wall). The slot (`#0`, `#14`) is the supervisor's index,
  fixed for the Linux engine, and is dropped from the row and its spoken label.
- **A status icon, not a dot** (owner, 2026-10-31): the dot was small and told states apart
  by colour alone. The icon's shape carries the state: `Activity` serving, `Moon` idle,
  `Hourglass` booting or proving the wall, `Power` stopping, `CircleQuestionMark` unmanaged.
  The moon is revisited if idle-stop (ADR-0052) adds a sleeping VM.
- **Stale draws no state** (owner, 2026-10-31): while the list is stale, every row's icon is
  a grey `CircleDashed` (status `unknown`), and the State line is left out. CPU and memory stay:
  they do not change while a VM runs.
- **One open row in the left column** (owner, 2026-10-31): opening a VM shuts an open gate,
  and the reverse. The jobs column keeps its own open group.
- **Two idle macOS VMs may read the same** (owner, 2026-10-31): they are interchangeable, their
  ages nearly always differ, and a number by position would move when one starts serving.
- **The section shows whenever a VM is up, idle popover included.** A running VM holds its
  memory while idle (AGENTS.md: a Linux VM never gives memory back), and the section is what
  explains a memory bar that is not empty with no job running. This changes `popover.md`
  § Healthy idle, which listed only the header, gauges and Pause.
- **No VM, no section.** No "no VMs" line: a healthy idle popover stays short
  (`popover.md` § Its one job).
- Rejected: **B, VMs as shares of the gates.** The memory gate measures what faber reserved
  for admission, not VM sizes, so Docker jobs inside the Linux VM would be a share of a share.
  It also needed accent tints that `tokens.css` has no roles for.
- Rejected: **C, a VM table.** No room for the job each VM serves, and it reads heavier
  than anything else in the popover.
- Rejected (kept from before): VM state as words in a caption line. It is what this
  replaces.
- **O1, where sizes come from: the daemon joins them in** (owner, 2026-10-29). The Linux
  VM's size comes from `[linux_vm]` (`EngineConfig`). A macOS VM's comes from its claim →
  in-flight runner → profile `cpus` / `memory_mib`. No protocol change. A VM adopted after a
  restart whose claim matches no runner shows no spec line.
  - Rejected: adding `cpus` / `memory_mib` to `VmRecord`. It is `deny_unknown_fields`, so
    that is a protocol bump with daemon/supervisor skew to handle.
- **O2, refresh: a 15 s poll everywhere** (owner, 2026-10-29). The daemon publishes the list
  on every supervisor `list` it already makes, and polls `list` every 15 s on any host that
  runs a VM, Linux engine included.
  - Before this, it was published only by the macOS sweep every 60 s, and never on a host
    whose only VM is the Linux engine.
- **O3: stopped VMs are hidden** (owner, 2026-10-29). `stopping` shows, with the amber ring.
- **Diverges from the reference:** `rec-loading-*` draws a skeleton VM row "when the last
  snapshot had VMs". No VM skeleton is built, and the loading frame stays as it is today
  (owner, 2026-10-29). Why: the popover window keeps its React state between opens, so the
  loading frame appears only before the first snapshot, when there is no last snapshot, and
  that row could never show.
- **Separators are the `Pipe` block, not `·`** (owner, 2026-10-29). The first canvas drew `·`;
  guidelines § Copy bans it (lint `design-tokens/no-dot-separator`) and the live job rows already
  use `Pipe`. The reference images were redrawn with pipes before the build.
- **No bare default state** (owner, 2026-10-29, design review on #428): a VM with nothing to do
  shows no text after its slot (`Linux #0`), per the guidelines' general rule No bare default states.
  The neutral dot says it. The first canvas drew `idle`; `rec-idle-*` was redrawn.
- **Stale rows show the name only** (owner, 2026-10-29), as `rec-supervisor-unreachable-*` draws:
  the last activity may be wrong, and the label's `as of` already says the list is old.
- **"Other hosts" is labelled whenever VM rows sit above it** (owner, 2026-10-29), idle included,
  so a stale peer row is never read as a VM.
- Open: none.

## 3. Code archaeology

- `src/limiter.rs` `Limiter::observe_vms` (≈ l. 1632) — the only writer of
  `vm.vms`. It maps `VmRecord` → `VmRow { slot, kind, state, stop_reason }` and **drops
  `claim` and `started_unix`**, the two fields the row's job name and age need.
- `src/limiter.rs` `pub struct VmRow` (≈ l. 4016) — what `/api/state` serializes. Display
  only: nothing gates on it.
- `src/vm/adopt.rs` `sweep_loop` / `SWEEP_INTERVAL = 60 s` — the only caller of
  `observe_vms`. It is spawned in `src/main.rs` (≈ l. 1190) only `if runs_vms`, and
  `runs_vms` means "a profile has `backend = macos-vm`" (≈ l. 853). The Linux engine alone
  does not set it. See O2.
- `src/vm/proto.rs` `VmRecord` — `slot, kind, claim, state, stop_reason, failed,
  started_unix`. No sizes: only `Start` carries `cpus` / `memory_mib`.
  `deny_unknown_fields` makes adding a field a protocol change (O1).
- `src/vm/engine.rs` — the Linux VM is one long-lived VM in `LINUX_SLOT` under the fixed
  claim `ENGINE_CLAIM`. Its size is `EngineConfig { cpus, memory_mib }` from `[linux_vm]`,
  with defaults `DEFAULT_LINUX_VM_CPUS` / `DEFAULT_LINUX_VM_MEMORY_MIB`.
- `src/vm/job.rs` (≈ l. 1425) — a macOS VM's claim is `claim_id(runner_name)`
  (`src/docker.rs`: the runner name without `faber-`). That is the join key from a VM to its
  in-flight job, and from the job to its profile.
- `src/config.rs` `FileProfile` — `cpus` / `memory_mib` per `macos-vm` profile, defaulting
  to `DEFAULT_VM_CPUS` / `DEFAULT_VM_MEMORY_MIB`.
- `src/limiter.rs` `DiskView.vm_free_mib` / `vm_reading_age_s` — the Linux VM disk's free
  space, already serialized. The UI type `Budget.disk` in `ui/src/popover/types.ts` does not
  declare it yet. A macOS VM's clone size is tracked nowhere, so its row has no disk.
- `ui/src/popover/peers.ts` `vmStates` / `VM_STATE_TEXT` — the caption's text. Retired by
  this change. `src/web.rs` (≈ l. 2258) has its own `VM_STATE_TEXT` for the web dashboard,
  which is out of scope (§ 7).
- `ui/src/screens/Popover.tsx` (≈ l. 271) — `const vms = idle ? [] : vmStates(...)`. The
  `idle ? []` is the reason VMs vanish when the popover is idle. The new section must not
  inherit it. It also feeds `idle` (Healthy idle collapses the card), and VMs must not make
  a popover non-idle: they are shown, not attention.
- Untrusted text (`popover.md` § Untrusted text) — the job name on a macOS row is GitHub's
  text, set as a text node. For a fork or untrusted-actor job the row names the repo only,
  as the attention row does.

## 4. Component inventory

| Element on artboard | Component | Status | Import | Fidelity | Artboards |
|---|---|---|---|---|---|
| Section divider | `<hr>` as today (`mx-xl border-border`) | reuse | — | as-drawn | rec-wide, rec-idle, rec-unreachable |
| Label "Virtual machines" + total / "as of 3m ago" | `VmSection` header line | new | `ui/src/blocks/VmSection.tsx` | as-drawn | rec-* |
| A VM row | `VmRow` | new | `ui/src/blocks/VmRow.tsx` | as-drawn | rec-wide, rec-idle, rec-unreachable |
| State dot (filled) | `StatusDot` | reuse | `ui/src/kit` | as-drawn | rec-wide, rec-idle |
| State ring (booting, stopping) | `StatusDot` + `ring` | extend | `ui/src/kit` | as-drawn | rec-wide |
| Age, right column | the `w-age` mono column, as PeerRow | reuse (classes) | — | as-drawn | rec-* |
| Separators in name, spec and total | `Pipe` | reuse | `ui/src/blocks` | as-drawn | rec-* |
| Loading skeleton row | not built | — | — | **diverges → § 2** (confirmed 2026-10-29) | rec-loading |
| Supervisor attention row | `AttentionRow`, the existing `update.supervisor_restart` row (its own wording) | reuse | `ui/src/blocks` | as-drawn (the artboard's wording is illustrative) | rec-unreachable |

## 5. Components

### `StatusDot` — `ring` (extend)

- **What:** a hollow dot for a state in transition (booting, stopping), beside a filled dot
  for a settled one.
- **API:** `ring?: boolean` (default `false`). The circle draws as a stroke in the tone's
  colour, no fill. It does not combine with `pulse`; `ring` wins.
- **Token map:** stroke `stroke-amber` (tone `amber`), stroke width 1.5 in the 8-unit
  viewBox, `r` 2.25. Size `size-sm` as today.
- **States:** add `ring` to `@states`, plus a story `Ring`.
- **Tests:** a kit test that `ring` renders a stroked, unfilled circle. Catalogue regenerated.

### `VmRow` (new block)

*Amended 2026-10-31 (owner):* a `Disclosure` row, status icon instead of `StatusDot`, and the
spec line moved into the open row as `facts` (`{ label, value }[]`: State, CPU, Memory, Disk free).
`doing` holds only what it serves; no `#slot` on the row.
`VmRowModel` is `{ key, kind, slot, doing, status, tone, facts, age, label }`; `ring`,
`spec` and `free` are gone. With no facts the row does not open. Props add `open` and
`onOpenChange`; the screen keeps one VM open at a time (`defaultOpenVm` in stories). The
bullets below are the original 2026-10-29 spec, kept for the record.

- **What:** one VM. Kind and slot, what it is doing, its age, and a mono spec line of what
  it was given.
- **Props:** `vm: VmRowModel` and `dim?: boolean`, the last known list while the supervisor is
  unreachable. `VmRowModel` is shaped in `ui/src/popover/vms.ts`:
  `{ key, kind: 'Linux' | 'macOS', slot, doing, tone, ring, spec, age, label }`.
- **Anatomy:** two rows. Row 1: dot, name, age. Row 2, under the name and spanning
  to the age column: the spec.
  - name: `Linux` / `macOS` in `font-medium text-fg`, then `#0 | <doing>` in
    `text-small text-faint`, with `Pipe` between.
  - spec: `font-mono text-caption text-muted tabular-nums`, e.g.
    `6 vCPU | 16 GiB | 40 GiB free`, with `Pipe` between parts. The free-disk part is in
    `text-faint`, Linux only.
  - age: `w-age text-right font-mono text-caption text-faint`. `—` when unknown.
- **`doing`**, the one text after the slot:

  | state | Linux | macOS |
  |---|---|---|
  | `in_service`, `job_finished` | `3 jobs` (`plural`, from `docker_inflight`); nothing at 0 | the job name (the repo for a fork PR); nothing with no job |
  | `starting`, `booting`, `ready` | `booting` | `booting` |
  | `probe_sent` | `proving the wall` | `proving the wall` |
  | `stopping` | `stopping` | `stopping` |
  | `unmanaged` | `unmanaged` | `unmanaged` |
  | `stopped` | hidden (O3) | hidden (O3) |

- **Tone:** `green` when it is serving a job (Linux with `docker_inflight > 0`, macOS with a
  job). `neutral` when up with nothing to do. `amber` + `ring` for booting, proving the wall
  and stopping. `neutral` for unmanaged. With `dim`, every dot is `neutral` and the age is `—`.
- **States** (one story each): serving (Linux, 3 jobs), serving (macOS, job name), idle,
  booting (ring), stopping (ring), unmanaged, no spec (size unknown, § 2 O1), long job name
  (truncates), dim.
- **Responsive:** the same at 240 and 308 px. The name line truncates with an ellipsis. The
  spec line truncates at its end, so `40 GiB free` is cut first, as `rec-wide` shows.
- **A11y:** `role="group"`, `aria-label` = `label`, e.g. "Linux VM 0, serving 3 jobs, 6 vCPU,
  16 GiB, 40 GiB disk free, up 2 hours". Not focusable, not pressable: nothing opens under it.
- **Content:** sizes through a new `gib(mib)` helper in `ui/src/popover/format.ts`, with a
  unit test: MiB ÷ 1024, no decimal when whole, one decimal otherwise (`8 GiB`,
  `12.5 GiB`). Age via `fmtAgo` (now − `started_unix`), as drawn: `2h`, `12m`, `40s`. Job name
  as a text node, max one line.
- **Motion:** none. `StatusDot` `pulse` is not used.

### `VmSection` (new block)

*Amended 2026-10-31 (owner):* no `total` prop or model field; the label's right side is only
`asOf` while stale. Props add `openKey` and `onOpenChange`.

- **What:** the label line and its rows. The Screen decides whether to render it at all.
- **Props:** `rows: VmRowModel[]`, `total: string[] | null` (`['14 vCPU', '32 GiB']`, joined by `Pipe`), `asOf: string
  | null` (`as of 3m ago`, set while the list is stale), `dim: boolean`.
- **Label line:** `px-xl pt-2xs pb-3xs`. "Virtual machines" in `text-caption font-semibold
  text-faint`. At the right, `font-mono text-caption text-faint`: `asOf` when set, else
  `total`. `total` is omitted when no row has a size.
- **States:** default (3 rows, wide), one row (idle, narrow), stale (`dim` + `asOf`), no
  sizes (no total).
- **A11y:** a `<section aria-label="Virtual machines">` with its rows.

### Data contract (daemon, Rust)

- `VmRow` gains `started_unix: u64`, `cpus: Option<u16>`, `memory_mib: Option<u32>` and
  `job: Option<VmJob>`, where `VmJob { repo, job_name: Option<String> }`. The job name is
  `None` for a fork or untrusted-actor job. `None` means not known; the UI then omits that
  part.
- `VmView` gains `listed_age_s: Option<u64>`, the age of the last successful supervisor list,
  for `asOf`. The UI shows `asOf` once it is over 2 × the poll interval.
- Sources: § 2 O1 (sizes, job) and O2 (freshness). The 15 s poll lives beside the macOS sweep in `src/vm/adopt.rs` and is spawned for any host with a Linux engine or a `macos-vm` profile; the 60 s sweep keeps its own job.
- `ui/src/popover/types.ts` mirrors all of these, and adds `vm_free_mib` to `Budget.disk`.

## 6. Composition

- **Order in the left column:** gates → `<hr>` → Virtual machines → `<hr>` → Other hosts.
  When there is no peer, the second `<hr>` and Other hosts are absent. When there is no VM,
  the first `<hr>` and the section are absent, and Other hosts follows the gates as today.
- **Row order:** Linux first, then macOS by slot.
- **Rows shown:** every non-stopped VM. The macOS cap (`[macos_vm] count`, 2 by default)
  plus one Linux VM bounds the list, so no "+N more" is needed.
- **Height:** the left column grows by about 34 px per row. The job groups' fold already
  follows the left column's height (`popover.md` § Two columns), so more groups show in full.
- **Narrow card (308 px):** the same section, same order, full width.
- **The Other hosts label** shows whenever the popover is not idle, or VM rows sit above it.
- **Supervisor unreachable:** the existing attention row says it. The section keeps the last
  list with `dim` and `asOf`.

## 7. Out of scope

- The full panel (`desktop/src`) and the web dashboard's VM text (`src/web.rs`
  `VM_STATE_TEXT`). They keep their own VM lines until the dashboard shell (ADR-0054) gets a
  Machines view.
- The other host's VMs. A peer is a runner count (ADR-0004), and its VMs are not in this
  host's snapshot.
- Per-VM actions or logs on click, such as a stop button or wall verdicts. A row opens
  only to list what the VM was given (2026-10-31).
- Live usage inside a VM (used vs given). The row shows the allocation only.
- A macOS VM's disk: its clone size is tracked nowhere.
