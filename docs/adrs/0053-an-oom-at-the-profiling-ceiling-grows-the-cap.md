---
title: An OOM at the profiling ceiling grows the cap; past the threshold, faber asks
type: adr
status: accepted
date: 2026-10-28
owner: rejifald
---

# ADR-0053: An OOM at the profiling ceiling grows the cap; past the threshold, faber asks

## Status

accepted (2026-10-28, owner). Closes #304 and #357. Amends the escalation guard in
[docs/autolimiter.md](../autolimiter.md) (*Recommendations & escalation*). The
guard's rule that the interim is a clamp, never a floor, stands unchanged.

## Context

The autolimiter learns a per-class cap from each container's peaks. An OOM is a
censored sample: the cap the container was killed at, times 1.5, so the want rises
past it. `apply_want` then clamped every container to `profiling_cap_mib`, OOM or
not.

That clamp exists for a good reason. Without it, the guard once gave a dind that
never used more than a few hundred MiB the whole ceiling. But for a container that
was killed *at* the ceiling, the clamp hands back the same cap that killed it, every
run.

Measured on one host in October 2026: one class of a watched repo was
OOM-killed on most of its recent runs, with peaks reaching the profiling ceiling it
was capped at. The only signal was a line in the budget warnings, of the form
"learned reservation <want> MiB exceeds 50% of budget (<threshold> MiB) — held at
<cap> MiB … pin to allow". Nobody reads that list. CI failed on every run.
Auto-retry (ADR-0003) retried each death, at the same clamped cap.

A quieter variant had no warning at all. A want between the ceiling and the
escalation threshold was clamped to the ceiling and still shown as `learned`.

## Decision

1. **An OOM or a peak above the ceiling lifts a container's ceiling to the
   escalation threshold.** A class's window may hold an OOM for a container, or a
   measured peak above `profiling_cap_mib`. A capped container can reach such a
   peak only under a cap already grown past the ceiling. Either way, that
   container's ceiling is no longer `profiling_cap_mib`. It becomes
   `budget × escalation_frac`, less the other container's cap, so runner + dind
   stays within the threshold. When both containers have that proof, the
   threshold is shared in proportion to their wants. Growth never lowers a cap
   below the profiling ceiling. The peak counts as proof so that a grown cap
   holds: with OOMs alone, the cap would drop back to the ceiling once the last
   OOM aged out of the window, and the class would OOM again once per window.
   One consequence: lowering `profiling_cap_mib` does not pull an already-grown
   class back, since its peaks above the new ceiling are proof too. Pin it, or
   let its window turn over.
2. **Without that proof, the clamp is unchanged.** A want above the ceiling that
   comes from peaks below it (peak × margin) is not evidence that the ceiling is
   too low. It still clamps exactly as before, and stays `learned` below the
   threshold, because nothing is failing. The quiet variant above was only a
   fault when the want came from an OOM, and decision 1 grows it.
3. **Past the threshold, faber holds the cap and asks the owner.** A want above
   the threshold is held there and marked `warned`. While an OOM is among the
   class's last 5 runs, `/api/state` lists the class in `budget.oom_held`. The
   popover shows it as a red attention row, and the dashboard as a red note:
   "OOM-killed on 8 of its last 10 runs: <class> — held at 12 GiB", both
   figures runner + dind. The entry
   carries the caps a pin needs (`pin_runner_mib`, `pin_dind_mib`: each killed
   container at its want, the other at its cap, never past the whole budget), and the popover row's **Pin at
   N GiB** button sets them after an in-row confirmation, since it grants past
   the threshold (the safer-option rule, #385); an older daemon's row says it is "the most faber
   grants unpinned. Pin it higher, or make the job lighter" instead. An OOM still in the window but older than 5 runs
   keeps the cap up, and asks nothing.
4. **A pin overrides all of this**, as before.
5. **The first measured budget re-evaluates every class.** The startup replay runs
   before the budget probe, so it has no threshold to grow to. Without this step,
   a restart would put an OOM-proven class back at the ceiling for one more run
   that is certain to fail.

## Consequences

- A class that is killed at the ceiling now gets the memory it needs on its next
  run, up to the threshold, without anyone pinning it.
- One class can now reserve up to the escalation threshold (half the budget by
  default) without a pin, where before it could take two profiling ceilings at
  most. That trade is deliberate. The threshold was always the line between
  auto-apply and asking a human; the ceiling only enforced it for one container.
- A job that really needs more than the threshold stays red until the owner acts,
  but now the owner is told where they look, not only in a log line.
- A grown cap also raises the cohort ceiling of a runner that might be handed that
  class. This is the same rule as for any learned cap (issue #13).
- No config key changes. An absent key behaves as before for every class that
  never OOMed. A class that OOMs gets more memory, which is the fix itself, so
  nothing needs a staged default.
