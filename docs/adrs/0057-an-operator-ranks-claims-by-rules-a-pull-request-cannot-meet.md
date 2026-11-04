---
title: An operator ranks claims by rules a pull request cannot meet
type: adr
status: proposed
date: 2026-10-31
owner: rejifald
---

# ADR-0057: An operator ranks claims by rules a pull request cannot meet

## Status

proposed (2026-10-31). Extends
[ADR-0036](0036-an-operator-can-put-repos-ahead-in-the-admission-queue.md): its two
groups become an ordered list of tiers, and `priority_repos` becomes shorthand for one
tier. Reverses ADR-0036's rejection of "several tiers". Gives macOS VM jobs
(ADR-0026, ADR-0051) a queue: today they have no admission order, and ADR-0036 does not
reach them. Applies on restart, like `priority_repos`.

## Context

ADR-0036 lets the operator put whole repos ahead in the Docker admission queue. That
answers "repo A matters more than repo B". It does not answer the next question the
operator asks: "some *jobs* matter more than others". In practice:

- A push to the default branch, a merge-queue run or a release deploys or unblocks
  everyone. A pull request's run unblocks one author.
- A merge-queue run that waits holds up every PR queued behind it. Under budget pressure
  today it parks behind whichever claims arrived first, PR runs from other repos
  included.

So the operator wants to rank a claim by **what kind of run it belongs to**, not only by
its repo.

Two facts about faber constrain how that can work.

**1. Anything a pull request can write, it can use to promote itself.** ADR-0036 kept
the repo list out of `.faber/config.toml` for this reason, and the same reasoning
applies to every per-job signal faber could read:

| Signal | Who controls it | Usable |
|---|---|---|
| repo | the operator's host config | yes (ADR-0036) |
| run `event` | GitHub, from what triggered the run | yes |
| `head_branch` on `push`, `merge_group`, `release`, `workflow_dispatch` | whoever can push to the base repo (write access) | yes |
| `head_branch` on a `pull_request*` event | the PR author, including a fork's | **no**: a fork can name its branch `main` |
| `head_branch` on a `workflow_run` event | inherited from the triggering run, which a fork PR can cause | **no**: the fork's branch name carries over |
| workflow or job name | the workflow file, which for `pull_request` comes from the PR head | **no** |
| `runs-on` label, `.faber/config.toml` | the PR head | **no** |

The `event` field cannot be forged from a PR. A fork PR cannot cause a `push`,
`merge_group`, `release` or `workflow_dispatch` run in the base repo. Those need write
access or a maintainer's action. faber already carries the run's `event` and
`head_branch` on every claim (`WorkflowRun`, `src/github.rs`), and the gate already
trusts `event` for its own decisions (ADR-0029, ADR-0030).

**2. A runner is not pinned to the job it was minted for** (ADR-0002, ADR-0017,
ADR-0029). faber mints a JIT runner per queued job, but GitHub hands that runner
whichever queued job of the repo matches its labels. Admission therefore decides
*which claim's runner starts*, not *which job runs on it*. This sets what a job-level
rank can and cannot do:

- **Across repos it is exact.** Runners are registered to one repo, so admitting repo
  A's merge-queue claim before repo B's PR claim puts the capacity in repo A.
- **Within one repo it does not reorder jobs that share labels.** If one repo has a
  merge-queue job and a PR job queued on the same labels, ranking the merge-queue claim
  first starts a runner sooner, but GitHub may give that runner the PR job. Within a
  repo the rank changes when that repo gets capacity, not which of its jobs uses it.
  It does reorder jobs whose labels differ, because a runner only takes jobs that match
  its labels.

**3. macOS VM jobs have no queue at all.** A macOS job does not join the Docker
admission queue. It waits in `Limiter::vm_admit`, which loops on `vm_begin`. When
anything that gates it changes, `notify_waiters()` wakes every waiting macOS claim, and
the first to call `vm_begin` takes the slot. Nothing orders the waiters:

- There is no first-come-first-served guarantee, so an old claim can lose the race to a
  younger one again and again.
- ADR-0036 does not reach these claims. The orchestrator passes the priority flag only
  on the Docker path, so a listed repo's macOS jobs do not queue ahead.

The macOS class is small and slot-bound. At most `MACOS_VM_CAP = 2` macOS VMs run per
host (Apple's licence, enforced by Virtualization.framework), and each one also takes a
fixed share of host memory (`macos_vm_mib`). Every job runs in a fresh clone that is
deleted afterwards (ADR-0026). So a VM that is already running offers nothing to a
waiting job: it is busy with its own job and is destroyed when that ends. The shared
warm state, a login session (ADR-0051) and a verified golden image (ADR-0049), admits
any waiting macOS job equally. There is no locality to rank by. The only question is
who takes the next free slot.

The decision has to be useful under all three facts, and must not claim more than the
second one allows.

## Decision

### 1. `[[autolimit.priority]]`, an ordered list of rules

```toml
[[autolimit.priority]]            # tier 1: the merge queue and releases of these repos
repo  = ["owner/demo", "owner/tooling"]
event = ["merge_group", "release"]

[[autolimit.priority]]            # tier 2: pushes to main, any repo
event  = ["push"]
branch = ["main"]

[[autolimit.priority]]            # tier 3: everything else from this repo
repo = ["owner/tooling"]
```

- **A rule's position is its tier.** The first rule is the highest. A claim takes the
  tier of the **first** rule it matches. A claim that matches no rule is in the lowest
  tier, below every rule. There are no numbers to tune: to reorder tiers, reorder the
  rules.
- **A rule matches when every field it states matches.** A field it leaves out matches
  anything. A rule must state at least one field. A rule with no fields would put every
  claim into one tier, which is the same as having no rules.
- **Fields:**
  - `repo`: `owner/repo` entries, case-insensitive, as in `priority_repos`. `owner/*`
    matches every repo of that owner.
  - `event`: GitHub event names, matched exactly against the run's `event`.
  - `branch`: branch names, matched exactly against the run's `head_branch`. **A
    `branch` never matches a run whose event starts with `pull_request`, or whose
    event is `workflow_run`**, because the PR author chose that name (a `workflow_run`
    run inherits the `head_branch` of the triggering run, which a fork PR can cause).
    A rule that lists `branch` together with a `pull_request*` or `workflow_run`
    event is refused when the config is validated, so it cannot sit in the file
    looking like it works. For `workflow_run`, only `event` plus `repo` are usable.
- **Order and set-aside work as in ADR-0036, generalised to N tiers.** Within a tier,
  claims keep their ticket order, first come first served. The head that the set-aside
  protects is the oldest claim of the highest tier present.

### 2. `priority_repos` is shorthand for one rule

`priority_repos = [...]` means one rule `{ repo = [...] }` placed **after** every
explicit rule. A host that sets only `priority_repos` behaves exactly as under ADR-0036.
A host that sets both gets its explicit tiers first, then the listed repos, then
everything else. Both keys stay valid. Neither is deprecated.

### 3. Every gate still applies, and nothing is pre-empted

This is unchanged from ADR-0036. A tier only decides who is the head:

- Memory Gates 1 and 2, CPU, disk, hold and the hard ceiling apply as they do today.
- A high-tier claim that cannot fit still waits.
- A high-tier head that cannot fit the whole budget is not set aside, because that
  would wedge the pool.
- An admitted or running job is never stopped or shrunk for a higher tier.
- Tiers apply in the Docker admission queue and in the macOS queue (decision 4).
  Native-backend jobs queue in neither and are unaffected.
- The two queues stay separate. A Docker claim never waits for a macOS claim, and a
  macOS claim never waits for a Docker one, whatever their tiers. They draw on
  different budgets: the Linux VM's ledger for Docker, and slots plus the host share
  for macOS. A tier ranks claims only against others that compete for the same thing.

### 4. macOS VM jobs queue in ticket order, by tier

`vm_admit` takes a ticket, as `admit_as` does, and the claim keeps it until it starts or
is dropped.

- **Only the head may start.** The head is the oldest ticket of the highest tier
  present. When `vm_begin`'s gates (hold, pause, machine pressure, wall, disk, session,
  image, cap, share) admit one more macOS VM, only the head takes it. Every other
  waiter is refused with `queued behind an older claim` or `queued behind a priority
  claim (rule N: …)`, the same wording as the Docker set-aside (decision 5).
- **No memory set-aside.** Every macOS VM takes the same `macos_vm_mib`, so the head
  never needs more than the next waiter. Handing the next free slot to the head is
  enough, and there is no larger or smaller claim that could slip in alongside it.
- **A head that cannot start does not block a waiter that can.** The gates that differ
  between claims are the image gate (ADR-0049: one golden image may be unverified while
  another is verified) and verification runs (`image = None`). If the head is refused
  for its own image's standing, the next waiter whose image admits it may start. This
  is the macOS form of ADR-0036's rule that an unsatisfiable head is not set aside.
- **Verification runs queue too, ahead of every tier.** A verification run is how an
  image becomes verified (ADR-0049), and until it passes every job on that image waits.
  It takes a ticket above the highest rule's tier. faber starts it, not a repo, so no
  PR can reach this place in the queue.
- **Adoption stays outside the queue.** `vm_adopt`, which counts a VM found running a
  job at boot, still counts unconditionally (ADR-0011). It is not an admission.
- **The wake stays `notify_waiters()`.** Every waiter re-checks, and only the head
  passes. A waiter that drops its future (a reclaim) leaves the queue, and the next
  ticket becomes the head, as `Ticket::drop` already does for Docker.

Because ADR-0036 never reached macOS, this also gives `priority_repos` an effect on
macOS jobs for the first time. A host that lists repos will see their macOS jobs queue
ahead after this change, where they raced as equals before.

### 5. The refusal names the rule

A claim set aside behind a higher tier says which rule that tier is:
`set aside for a priority claim (rule 2: event=push branch=main)`. The `priority_repos`
tier keeps ADR-0036's `set aside for a priority claim`, and a same-tier stall keeps
`set aside for an older claim`. A waiting macOS claim says `queued behind` in place of
`set aside for`, followed by the same text, or `queued behind an image verification`. A stalled claim then shows on the dashboard which rule
it is waiting behind, which is how the operator sees starvation (Consequences).

### 6. It is host-only

`[[autolimit.priority]]` lives in the host config and is refused by
`.faber/config.toml` (`RepoConfig` has `deny_unknown_fields`), for ADR-0036's reason.
No field of a rule can be met by a fork PR on its own: `repo` and the ranking are the
operator's, `event` is GitHub's, and `branch` is ignored for PR and `workflow_run`
events.

### 7. Shape in the code

`priority_tickets: BTreeSet<u64>` becomes a tier per ticket: `BTreeMap<u8, BTreeSet<u64>>`
keyed by tier, highest first. The head is the first ticket of the highest tier present.
`Limiter::admit_as(reservation, priority: bool, ..)` takes a `Tier`, and
`is_priority_repo(repo)` becomes `tier_for(repo, event, head_branch)`. The orchestrator
already holds all three at the admission call (`src/reconcile.rs`). Absent rules and
absent `priority_repos` give every claim the same tier, which is today's strict
first-come-first-served.

The macOS side gets its own `vm_tickets: BTreeMap<Tier, BTreeSet<u64>>` in the
limiter's VM state, separate from the Docker one (decision 3). `vm_admit(image, ..)`
becomes `vm_admit_as(image, tier, ..)`, taking a ticket that drops with the future.
`vm_begin_with_held` gets one more check after the gates: this ticket is the first
ticket, in tier order, among the waiters that the image gate admits. The two callers
today are the job path (`src/reconcile.rs`, which takes the tier from `tier_for`) and
the verification pass (`src/vm/verify.rs`, `vm_admit(None, ..)`, which takes the
verification tier).

## Consequences

- **Within one repo the rank is partly advisory, and that is accepted.** The tier
  decides when the repo gets capacity. For jobs that share labels it does not decide
  which of them uses it (Context, fact 2). The settings page and `docs/autolimiter.md`
  state this next to the key, so a rule like "push before pull_request in owner/demo"
  is not read as a guarantee. A guarantee would need runners pinned to their job, which
  GitHub's JIT runners do not offer. This ADR does not attempt it.
- **Starvation is easier to cause than under ADR-0036, and is still accepted without
  aging.** ADR-0036 expected a short list of small repos. An event rule like
  `event = ["push"], branch = ["main"]` matches every merge of every repo, so a busy day
  of merges can hold PR runs back for as long as merges keep arriving. The reasoning is
  ADR-0036's: the operator wrote the rule, the stall names the rule (decision 5), and
  aging would bring back the head-of-line delay the rule exists to remove. If this
  starvation shows up in practice, aging is the first revisit (Alternatives).
- **A high-tier head's set-aside can idle memory** while it waits for room, exactly as
  an old head's does today.
- **macOS admission changes on every host, config or no config.** Absent rules still
  give macOS waiters a queue, so the race of fact 3 becomes first-come-first-served.
  This narrows nothing: every claim that could start before can still start, only the
  order among waiters is fixed. That is why it needs no key, unlike AGENTS.md's case
  of a tightening default. A host that already lists `priority_repos` does see a
  change: those repos' macOS jobs now queue ahead (decision 4). The release notes say
  so.
- **A macOS slot can sit free for one wake.** If the head drops out (a reclaim) just
  as a slot frees, the slot waits for the next wake or `ADMIT_RETRY`. That delay is
  bounded and small next to a macOS boot.
- **A busy priority tier can hold back macOS jobs harder than Docker jobs.** With two
  slots, a steady stream of high-tier macOS jobs leaves lower tiers nothing until it
  stops. The reasoning for accepting starvation is the same as for Docker, and so is
  the signal: the `queued behind` message.
- **Writing `push` without `branch` is a deliberate choice.** It ranks every push by
  anyone with write access to that repo, feature branches included. That is still not
  PR-reachable, but it is broader than "main".
- **`workflow_run` is matchable by `event` (and `repo`) only, never by `branch`.** It is
  triggered by other runs, including a fork PR's completed run, and its `head_branch`
  is the triggering run's, so a fork can pick it. Its workflow file comes from the
  default branch, so the code it runs is trusted, but how often it runs is set by PR
  traffic. Listing `event = ["workflow_run"]` ranks that traffic. This is the
  operator's call and is documented, not refused; `issue_comment` is the same, its
  frequency being outsider-controlled, and it is likewise ranked by `event` alone.

## Alternatives considered

- **Labels or keys in the workflow or `.faber/config.toml`** (`runs-on: [faber,
  priority]`, `priority = 1`). Rejected: PR-reachable, so any PR, a fork's included,
  could promote itself (ADR-0036, Context table).
- **Matching on workflow or job name.** Rejected for v1. For `pull_request` runs the
  workflow file comes from the PR head, so the name is the author's. It is sound only
  together with a non-PR event, and `event` plus `branch` already express that case. It
  can be added later as a field that, like `branch`, never matches PR events.
- **Numeric weights.** Rule order already ranks the rules, and weights would add
  numbers to tune and ties to explain. ADR-0036 rejected weights for the same reason.
- **Aging** (a waiting claim climbs a tier over time). Rejected for now, as in ADR-0036.
  It is the first revisit if decision 5's messages show a lower tier waiting for hours.
- **A reserved budget slice per tier.** Rejected, as in ADR-0036: it idles memory
  whenever the tier is quiet, which is most of the time.
- **Resolving each repo's real default branch instead of naming `branch`.** It would
  need a repo lookup per claim or a cache. Naming the branch costs one word per rule, so
  the lookup is not worth it.
- **Preferring macOS jobs because a macOS VM is already running.** There is nothing to
  prefer. A VM runs one job in its own clone and is deleted afterwards (ADR-0026), so a
  running VM offers nothing to a waiting job and holds one of the two slots. The warm
  state that exists, the login session and the verified image, serves every waiter
  equally.
- **One queue across Docker and macOS.** Rejected: the two draw on different budgets,
  so a macOS head would hold Docker claims back for a slot they could never use, and
  the reverse.
- **Pinning runners to their job, so within-repo ranking is exact.** Not available:
  GitHub assigns queued jobs to matching runners itself.
