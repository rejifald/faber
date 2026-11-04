---
title: Demand-based provisioning for multi-instance faber
type: adr
status: accepted
date: 2026-07-25
owner: rejifald
---

# ADR-0004: Demand-based provisioning for multi-instance faber

## Status

accepted (2026-07-25) — implemented this session (see the diff on
`src/reconcile.rs`, `src/github.rs`, `src/config.rs`, `src/main.rs`). Opt-in and
off by default (`provisioning = "per-job"`), so existing single-instance
deployments are unchanged. Prompted by an operator goal: "run faber in parallel on
a second machine, so only one runner picks a job at a time." Realises the
**demand-based provisioning** [ADR-0002](0002-reconcile-inflight-claims.md)
deferred as the root-cause fix for surplus minting.

**Amended by [ADR-0018](0018-a-runner-is-named-when-it-registers.md) (2026-09-17).**
The offline-runner rule below later gained an age bound, read from the timestamp in
the runner's name, and a stale-runner sweep that reads the same timestamp. That
timestamp was taken before a claim parked for memory, so a runner whose claim had
parked registered already too old to count as connecting, and a sweep could delete
its registration before the listener connected. The name is now minted when the
claim is admitted, so a runner's age is the time since it registered.

## Context

faber's reconcile loop mints **one JIT runner per observed queued job**, deduped
only by this process's in-memory claim set (`inflight: job.id → Claim`). That set
has no persistence and no cross-machine visibility. Run a second faber instance
against the same repos and each instance independently sees the same queued job and
each mints a runner for it.

Two facts bound what that costs — and what it does *not*:

- **GitHub is the single job broker.** A queued Actions job is assigned to exactly
  one runner. Two instances (or two runners) can never make a job *execute twice*.
  The operator's literal worry — "only one runner picks a job at a time" — is
  already guaranteed upstream; it is not something faber must enforce.
- **The surplus is real but self-healing.** The instance whose runner GitHub does
  *not* pick has an idle runner. Under not-job-pinned assignment it may take a
  sibling job; otherwise it idles and is reclaimed within the
  [ADR-0002](0002-reconcile-inflight-claims.md) grace (~2 min), or killed at the
  lifetime cap. So naive parallel operation is **safe but wasteful** — roughly 2×
  runner churn, inflated dashboards, and needless memory-budget pressure.

  **No longer wholly true**, and worth reading with
  [ADR-0009](0009-runner-image-pool-coherence.md) alongside it. Once
  [ADR-0008](0008-on-demand-runner-image-variants.md) made the runner image a
  function of the job, a surplus runner stopped being merely wasteful: a job
  taken by a runner minted for a different job is taken by a container built for
  a different pin. ADR-0009 restores the "safe" half within one instance; across
  instances it still relies on peers advertising what they resolved, which is not
  built.

The operator wants the two machines to act as **one elastic pool** over the *same*
repos (not a static repo partition), with **no extra infrastructure** (no Redis,
no shared DB, no peer-to-peer protocol) — faber is a self-contained single binary
and should stay one.

The key enabling observation: **GitHub's runner registry is itself the shared
state.** Both instances already call `GET /actions/runners` every cycle (for
[ADR-0002](0002-reconcile-inflight-claims.md) reclaim). That list contains *both*
instances' runners. So each instance can subtract the runners that already exist —
including the peer's — and mint only the shortfall. The coordination point is
GitHub; no new component is introduced.

`generate-jitconfig` registers the runner **server-side at generation** (its
response carries a full `runner` object with `id`/`name`/`status`/`labels`), so a
freshly-minted runner is listable — as `offline` — before its process connects.
That makes a peer's in-flight mint visible almost immediately, which is what keeps
the coordination window small.

## Decision

Add a `provisioning` strategy knob (config file only): `per-job` (default,
unchanged) or `demand`. Under `demand`, per repo per cycle:

1. **Compute available supply per profile** from the repo's registered runners.
   A runner counts as supply when it carries all of a profile's labels
   (`match_profile` on the *runner's* labels, so it is attributed to the same
   profile faber would route it to) **and** is either:
   - `online` and **idle** (`!busy`) — it will pick a queued job up; or
   - `offline` with a `faber-{id}-` name whose `{id}` is **still live this cycle**
     (queued/in-progress) — a peer's (or our own) mint mid-registration, about to
     come online.

   An `online` + `busy` runner is already executing a job (not queued backlog); a
   **stale offline orphan** (its namesake job gone) is ignored, so a dead
   registration cannot mask real demand; a non-faber offline runner is ignored (it
   may be permanently dead).

2. **Mint only the shortfall.** Iterating a profile's gated, unclaimed, queued
   jobs, the first *S* are treated as covered by existing supply (skipped, one
   supply unit spent each); the rest mint as before. A job this instance already
   owns is skipped by the existing claim dedup *before* the supply check, so it
   never spends a slot.

3. **Fail open, never starve.** If `list_runners` errors, that repo falls back to
   `per-job` for the cycle (empty supply ⇒ every gated job mints). Missing supply
   data must never leave a queued job with no runner.

The pure decision core (`faber_job_id`, `runner_is_supply`, `compute_supply`, and
the supply gate inside `process_run_jobs`) is spawn- and network-free, mirroring
the [ADR-0002](0002-reconcile-inflight-claims.md) `parked_verdict` /
`admitted_verdict` split, so it is exhaustively unit-tested.

## Consequences

- **One elastic pool, no new infrastructure.** Two (or N) instances share the same
  repos and split the queue; the coordination substrate is GitHub's own runner
  list, which faber already fetches. The single-binary story is intact.
- **Root-cause fix, single-instance too.** Demand mode also suppresses the
  single-instance over-mint (one runner per queued job even when an idle registered
  runner could take it) that [ADR-0002](0002-reconcile-inflight-claims.md) could
  only mop up after the fact.
- **A bounded residual, accepted.** Two instances polling within the window before
  a fresh runner appears in the list both see zero supply and both mint — one
  surplus runner, idle-reclaimed as before. The window is seconds (a JIT runner is
  listable-as-offline at generation), and shrinks further with staggered poll
  offsets. **Correctness never rests on winning the race** — GitHub stays the sole
  broker; the worst case is a brief surplus (status quo) or a one-cycle-delayed
  mint, never a double-run or a permanently-starved job.
- **API cost: +1 `list_runners` per repo per cycle**, and only for repos with
  queued work in demand mode (a quiet repo triggers no extra call). Kept separate
  from the reclaim fetch so the reclaim decision always runs on a fresh list.
- **Honest-enough accounting.** A covered job is silently not minted (like a
  throttle back-off), with one summary `info!` per repo per cycle naming how many
  jobs were covered vs. minted, for live validation. `provisioned` counts only
  runners this instance actually mints.
- **New surface.** `Runner.labels` (now parsed from the API); `ProvisioningMode`
  (`config.rs`); the supply seam in `reconcile.rs`. Default `per-job` keeps every
  existing deployment byte-for-byte unchanged.

## Alternatives considered

- **Do nothing — rely on GitHub + idle-reclaim.** Correct, and genuinely fine if
  the ~2× surplus is tolerable. Rejected as the *default* answer only because the
  operator wants the waste gone; it remains exactly what `per-job` still does.
- **Static repo/topic partition.** Zero code, zero coordination — assign disjoint
  repo sets (by topic) to each machine. Rejected for *this* goal: it gives more
  total capacity but not one elastic pool, and can't load-balance a single busy
  repo across machines. Still the right call for a many-repo, capacity-only setup.
- **Hard lease on `job.id` via a shared store** (Redis `SET NX EX`, etcd, Postgres
  advisory locks). Deterministic mutual exclusion, but adds a stateful dependency
  the whole system then leans on (fail-open ⇒ duplicates, fail-closed ⇒ no CI) —
  disproportionate when GitHub already prevents double execution. Rejected for a
  two-node homelab; reconsider at larger scale or if the residual surplus ever
  matters.
- **Peer-to-peer claims over the existing `:8787` servers.** No external infra, but
  symmetric two-node agreement needs a leader/tie-break and split-brain handling —
  more failure modes than reading a list both instances already fetch. Rejected.
- **Counting *only* online-idle runners (ignore offline).** Simpler, no
  orphan-guard needed, but blind to a peer's runner during its ~10–20 s connect
  window, widening the double-mint race to the full connect time. Rejected: the
  live-namesake offline guard buys tight coordination cheaply.
- **Counting *all* offline faber runners as supply.** Tighter still, but a crashed
  peer's offline registration for a still-queued job would mask it until GitHub
  reaps the runner. The live-namesake guard is the middle ground; the remaining
  "peer crashed mid-registration for a still-live job" edge is a bounded delay, not
  a starve.

## Validation against real circumstances

To validate on the two-machine deployment (both watching the same repos, one with
`provisioning = "demand"` then both):

1. **No double-mint in steady state.** For a burst of *N* queued jobs across two
   demand-mode instances, the total runners minted ≈ *N* (not ~2N), and the
   per-repo `info!` shows jobs "covered by existing runners" on the trailing
   instance. Confirm via the audit `provisioned` count summed across both nodes and
   the dashboards.
2. **No starve on supply-read failure.** Kill one node's network briefly; the other
   must fall back to per-job (mint everything) rather than leave jobs queued.
3. **Orphan doesn't mask demand.** An `offline` runner whose namesake job has
   completed must not suppress a genuinely-new queued job (covered by the
   `demand_ignores_a_stale_offline_orphan` unit test; confirm live that a redeploy's
   leftover offline registration doesn't stall the next job).
4. **The empirical unknown — offline visibility timing.** Confirm with one
   `list_runners` immediately after a mint that a generated-but-unconnected JIT
   runner is listed as `offline` (the returned `runner` object makes this near
   certain, but GitHub's docs don't state the timing). If it turns out a JIT runner
   is *not* listed until it connects, demand mode stays correct — the double-mint
   race just widens to the connect time — and the mitigation is a staggered poll
   offset (or, later, having each instance advertise its parked claims).

_Live results: pending — to be recorded here after the two-machine rollout._
