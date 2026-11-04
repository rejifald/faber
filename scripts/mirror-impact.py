#!/usr/bin/env python3
# Measures CI network-fetch cost per job (checkout, LFS, node_modules seconds), split per faber host,
# for the mirror trial in ADR-0047: the host with the mirror on against a peer without it.
#   python3 -I scripts/mirror-impact.py --repo OWNER/REPO --since START --until END --out DIR
"""Measure per-job network fetch cost of CI (LFS, checkout, node_modules).
Usage: python3 -I scripts/mirror-impact.py --repo OWNER/REPO --since YYYY-MM-DD --until YYYY-MM-DD [--sample N] --out DIR
Read-only (GET only) via `gh`. Deterministic sampling (evenly spaced over runs sorted by id).

Step names and log lines differ per workflow, so what counts as the LFS step, the dependency step and
its cache markers is set per workflow: --lfs-step and --deps-step (regexes over step names), and
--deps-state, --deps-hit-text, --deps-end-line, --deps-ids, --remote-id, --install-id (what the job log
prints). See --help. Checkout and job seconds need none of them."""
import argparse, json, os, re, subprocess, statistics, sys, time
from datetime import datetime

def gh(args, text=True):
    r = subprocess.run(["gh"] + args, capture_output=True, text=True)
    return r.returncode, r.stdout, r.stderr

def api_json(path):
    rc, out, err = gh(["api", path])
    if rc: return None
    try: return json.loads(out)
    except Exception: return None

def rate_guard(min_remaining=500):
    d = api_json("rate_limit")
    if not d: return
    c = d["resources"]["core"]
    if c["remaining"] < min_remaining:
        wait = max(0, c["reset"] - time.time()) + 5
        print(f"rate limit low ({c['remaining']}), sleeping {wait:.0f}s", file=sys.stderr)
        time.sleep(wait)

UNIT = {"B":1,"KB":1e3,"MB":1e6,"GB":1e9,"KIB":1024,"MIB":1024**2,"GIB":1024**3}
def tobytes(n, u): return float(n) * UNIT[u.upper()]
def ts(s): return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() if s else None
def dur(st):
    a, b = ts(st.get("started_at")), ts(st.get("completed_at"))
    return round(b - a, 1) if a is not None and b is not None else None

LFS_RE = re.compile(r"Downloading LFS objects:\s+100%\s+\((\d+)/(\d+)\),\s+([\d.]+)\s*(B|KB|MB|GB|KiB|MiB|GiB)\b")
RECV_RE = re.compile(r"Receiving objects:\s+100%.*?,\s+([\d.]+)\s*(KiB|MiB|GiB|B)\b")
TS_RE = re.compile(r"^\S+Z ", re.M)

def analyse_log(log, want_lfs, want_nm, cfg):
    # NOTE: CI logs carry no byte counts (git/git-lfs suppress progress when not a tty), so
    # lfs_bytes/checkout_bytes are only parsed if present; otherwise null (LFS gets an estimate in main()).
    # cfg holds the per-workflow log markers (state_re, hit_text, end_re, ids, remote_id, install_id); each is
    # optional, and what it would have filled stays null without it.
    o = {"lfs_bytes": None, "lfs_objects": None, "checkout_bytes": None, "nm_result": None,
         "nm_restore_seconds": None, "nm_npm_ci_ran": None, "nm_remote_restore_ran": None}
    ms = LFS_RE.findall(log)
    if want_lfs and ms:
        o["lfs_bytes"] = sum(tobytes(m[2], m[3]) for m in ms[-1:]); o["lfs_objects"] = int(ms[-1][1])
    rs = RECV_RE.findall(log)
    if rs: o["checkout_bytes"] = sum(tobytes(a, b) for a, b in rs)
    if want_nm:
        ends = {m[0]: (m[1], int(m[2])) for m in cfg.end_re.findall(log)} if cfg.end_re else {}
        m = cfg.state_re.search(log) if cfg.state_re else None
        o["nm_result"] = {"HIT": "hit", "MISS": "miss"}.get(m.group(1).upper()) if m else ("hit" if cfg.deps_hit_text and cfg.deps_hit_text in log else "unknown")
        parts = [ends[k] for k in cfg.ids if k in ends]
        if parts: o["nm_restore_seconds"] = round(sum(p[1] for p in parts) / 1000, 1)
        o["nm_remote_restore_ran"] = ends.get(cfg.remote_id, ("skipped",))[0] != "skipped" if cfg.remote_id in ends else None
        o["nm_npm_ci_ran"] = ends.get(cfg.install_id, ("skipped",))[0] != "skipped" if cfg.install_id in ends else None
    return o

def pct(v, p):
    v = sorted(v)
    if not v: return None
    k = (len(v) - 1) * p; f = int(k); c = min(f + 1, len(v) - 1)
    return v[f] + (v[c] - v[f]) * (k - f)

def stats(v):
    v = [x for x in v if x is not None]
    if not v: return (0, None, None, None)
    return (len(v), sum(v), statistics.median(v), pct(v, 0.9))

def fmt(x, kind):
    if x is None: return "n/a"
    if kind == "B": return f"{x/1e6:.1f} MB"
    return f"{x:.1f}"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True); ap.add_argument("--since", required=True)
    ap.add_argument("--until", required=True); ap.add_argument("--sample", type=int, default=60)
    ap.add_argument("--out", required=True)
    ap.add_argument("--lfs-estimate-mb", type=float, default=None, help="size of the LFS include set at HEAD in MB, used for the estimate (logs print no bytes); without it there is no estimate")
    # Everything below is set per workflow: step names and log lines belong to one repo's CI.
    ap.add_argument("--lfs-step", default="lfs", help="regex (case-insensitive) over step names: the step that fetches LFS objects")
    ap.add_argument("--deps-step", default="install dependencies", help="regex (case-insensitive) over step names: the step, possibly a composite one, that restores or installs node_modules")
    ap.add_argument("--deps-state", default=None, help="regex over the job log with one group, HIT or MISS: the line where the cache reports whether node_modules came from it")
    ap.add_argument("--deps-hit-text", default=None, help="text in the job log that means a hit when --deps-state finds no line")
    ap.add_argument("--deps-end-line", default=None, help="regex over the job log with three groups (step id, outcome, duration in ms): the end line of each inner step of a composite step, which the jobs API does not list")
    ap.add_argument("--deps-ids", default="", help="comma-separated step ids (group 1 of --deps-end-line) whose durations add up to the restore/install seconds")
    ap.add_argument("--remote-id", default=None, help="step id (as above) of the restore from a remote cache; reported as ran or skipped")
    ap.add_argument("--install-id", default=None, help="step id (as above) of the dependency install (npm ci or similar); reported as ran or skipped")
    a = ap.parse_args()
    a.lfs_re = re.compile(a.lfs_step, re.I)
    a.deps_re = re.compile(a.deps_step, re.I)
    a.state_re = re.compile(a.deps_state) if a.deps_state else None
    a.end_re = re.compile(a.deps_end_line) if a.deps_end_line else None
    a.ids = [i for i in a.deps_ids.split(",") if i]
    os.makedirs(a.out, exist_ok=True)
    rate_guard()
    # run enumeration
    runs = {}
    for ev in ("push", "pull_request", "schedule", "workflow_dispatch"):
        rc, out, err = gh(["run", "list", "-R", a.repo, "--event", ev, "--status", "completed",
                           "--created", f"{a.since}..{a.until}", "--limit", "1000",
                           "--json", "databaseId,event,workflowName,conclusion"])
        if rc: print(f"run list {ev} failed: {err}", file=sys.stderr); continue
        for r in json.loads(out): runs[r["databaseId"]] = r
    ids = sorted(runs)
    total = len(ids)
    if total > a.sample:
        step = total / a.sample
        sel = [ids[int(i * step)] for i in range(a.sample)]
    else: sel = ids
    print(f"{total} runs in window, sampling {len(sel)}", file=sys.stderr)
    rows = []; logs_tried = logs_failed = 0
    for n, rid in enumerate(sel):
        if n % 20 == 0: rate_guard()
        d = api_json(f"repos/{a.repo}/actions/runs/{rid}/jobs?per_page=100")
        if not d: continue
        for j in d.get("jobs", []):
            if j.get("status") != "completed" or j.get("conclusion") == "skipped": continue
            steps = j.get("steps", [])
            def sel_steps(pred): return [s for s in steps if pred(s["name"])]
            co = sel_steps(lambda n: n.lower().startswith("checkout") or "actions/checkout" in n.lower() or n == "Run actions/checkout@v4" or n.startswith("Run actions/checkout"))
            lfs = sel_steps(lambda n: bool(a.lfs_re.search(n)))
            nm_restore = sel_steps(lambda n: bool(a.deps_re.search(n)))  # may be composite: inner steps only visible in the log
            def ran(s): return s.get("conclusion") not in ("skipped", None)
            row = {"run_id": rid, "workflow": runs[rid]["workflowName"], "event": runs[rid]["event"],
                   "job_id": j["id"], "job": j["name"], "runner": j.get("runner_name"),
                   "faber": (j.get("runner_name") or "").startswith("faber-"),
                   # faber runner names end in the host's instance suffix (`-builder`, `-follower`), so
                   # stages enabled on one host only can be compared host against host. Names minted before
                   # the suffix existed end in the mint timestamp instead: those are "unsuffixed".
                   "host": (lambda n: "other" if not n.startswith("faber-") else
                            ("unsuffixed" if n.rsplit("-", 1)[-1].isdigit() else n.rsplit("-", 1)[-1]))(j.get("runner_name") or ""),
                   "job_seconds": dur(j),
                   "checkout_seconds": sum(filter(None, (dur(s) for s in co if ran(s)))) if co else None,
                   "lfs_seconds": sum(filter(None, (dur(s) for s in lfs if ran(s)))) if lfs else None,
                   "nm_seconds": None,
                   "has_lfs_step": bool(lfs), "has_deps_step": bool(nm_restore),
                   "lfs_bytes": None, "lfs_objects": None, "checkout_bytes": None, "nm_result": None, "nm_restore_seconds": None, "nm_npm_ci_ran": None, "nm_remote_restore_ran": None}
            if lfs or nm_restore:
                logs_tried += 1
                rc, log, err = gh(["api", f"repos/{a.repo}/actions/jobs/{j['id']}/logs"])
                if rc or not log: logs_failed += 1
                else: row.update(analyse_log(log, bool(lfs), bool(nm_restore), a))
            row["nm_seconds"] = row.pop("nm_restore_seconds")
            if row["nm_seconds"] is None and not a.end_re and nm_restore:
                # No per-step end lines to read: a plain dependency step's own wall-clock is the cost.
                row["nm_seconds"] = sum(filter(None, (dur(s) for s in nm_restore if ran(s))))
            # LFS bytes ESTIMATE: logs print no sizes; an LFS step that ran fetches the whole include set
            # into a fresh container (no persistent LFS store), so bytes ~= --lfs-estimate-mb per such job.
            row["lfs_bytes_estimated"] = a.lfs_estimate_mb * 1e6 if a.lfs_estimate_mb is not None and lfs and any(ran(s) and s.get("conclusion") == "success" for s in lfs) else None
            rows.append(row)
    with open(os.path.join(a.out, "raw.jsonl"), "w") as f:
        for r in rows: f.write(json.dumps(r) + "\n")
    # summary
    scale = total / len(sel) if sel else 0
    L = [f"# CI network-fetch baseline: {a.repo}", "",
         f"- Window: {a.since} .. {a.until}; runs in window: {total}; sampled: {len(sel)} (scale x{scale:.2f}); jobs: {len(rows)}",
         f"- Logs fetched: {logs_tried}, unavailable: {logs_failed}",
         f"- faber-runner jobs: {sum(r['faber'] for r in rows)} / {len(rows)}", "",
         "| metric | n | sum | median | p90 | extrapolated total for window |", "|---|---|---|---|---|---|"]
    def line(label, key, kind, filt=lambda r: True):
        n, s, m, p = stats([r[key] for r in rows if filt(r)])
        ext = fmt(s * scale, kind) if s is not None else "n/a"
        L.append(f"| {label} | {n} | {fmt(s, kind)} | {fmt(m, kind)} | {fmt(p, kind)} | {ext} |")
    line("LFS bytes, parsed from logs (usually none)", "lfs_bytes", "B")
    line("LFS bytes ESTIMATE (successful LFS step x lfs-estimate-mb)", "lfs_bytes_estimated", "B")
    line("LFS seconds", "lfs_seconds", "s")
    line("Checkout seconds", "checkout_seconds", "s")
    line("Checkout bytes (Receiving objects, if printed)", "checkout_bytes", "B")
    line("node_modules restore/install seconds (all)", "nm_seconds", "s")
    line("node_modules seconds, misses only", "nm_seconds", "s", lambda r: r["nm_result"] == "miss")
    line("node_modules seconds, hits only", "nm_seconds", "s", lambda r: r["nm_result"] == "hit")
    line("Job total seconds", "job_seconds", "s")
    L += ["", "## Per host", "", "| host | jobs | checkout median | checkout p90 | LFS median | LFS p90 |", "|---|---|---|---|---|---|"]
    for host in sorted({r["host"] for r in rows}):
        hr = [r for r in rows if r["host"] == host]
        _, _, cm, cp = stats([r["checkout_seconds"] for r in hr])
        _, _, lm, lp = stats([r["lfs_seconds"] for r in hr])
        L.append(f"| {host} | {len(hr)} | {fmt(cm, 's')} | {fmt(cp, 's')} | {fmt(lm, 's')} | {fmt(lp, 's')} |")
    nmr =[r["nm_result"] for r in rows if r["has_deps_step"]]
    h, m_, u = nmr.count("hit"), nmr.count("miss"), nmr.count(None) + nmr.count("unknown")
    L += ["", f"node_modules (cache tier): hit={h} miss={m_} unknown/no-log={u}; hit ratio (known) = {h/(h+m_):.1%}" if h + m_ else "node_modules: no data",
          f"Jobs where the dependency install ran: {sum(1 for r in rows if r['nm_npm_ci_ran'])}; remote cache restore ran: {sum(1 for r in rows if r['nm_remote_restore_ran'])}; of {len(nmr)} jobs with a dependency step",
          "", "## Extraction notes",
          "- LFS/checkout bytes: CI logs contain NO transfer sizes (progress suppressed, no tty); LFS bytes are an ESTIMATE, checkout bytes unavailable (null). Parse attempted for: last 'Downloading LFS objects: 100% (a/b), X MB' line (git-lfs, decimal units).",
          "- Checkout bytes: sum of 'Receiving objects: 100% ... X MiB' lines; null/0 when git prints none (non-tty logs often suppress progress).",
          "- node_modules hit/miss: the job-log line matched by --deps-state (or the text of --deps-hit-text); seconds = sum of the end-line durations (--deps-end-line) of the step ids in --deps-ids (composite inner steps are absent from the jobs API), else the dependency step's own wall-clock.",
          "- Seconds are step wall-clock from the jobs API (1 s resolution)."]
    open(os.path.join(a.out, "summary.md"), "w").write("\n".join(L) + "\n")
    print("\n".join(L))
main()
