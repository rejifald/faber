// faber's job-started hook (ADR-0059 §1).
//
// The runner runs this once a job is assigned to it, before any of the job's
// steps, as the step "Set up runner": ACTIONS_RUNNER_HOOK_JOB_STARTED names it,
// and the runner runs a `.js` hook on its own bundled Node, so nothing here needs
// a shell or anything else from the image. faber mounts it read-only and writes
// the policy line below when it starts the runner.
//
// It decides nothing itself. It waits for faber's verdict on this job, which
// faber writes into this container through the Docker API once it has found the
// job GitHub gave this runner and judged it with its gate (ADR-0059 §2). It parses
// no event payload and carries no gate policy beyond the one outcome faber
// decided at mint for a verdict that never comes.
//
// A failed hook's exit code does not stop a job: later steps whose condition is
// always() still run, and an action's pre-step defaults to always(). So on every
// path but an accepted allow, this prints one fixed reason line and kills its
// parent, the runner's worker, which runs as the same user. No later step can
// start then, whether or not faber is reachable. faber's own stop of the
// container is the cleanup.
//
// The reason line is one of a fixed set, because anyone who can read the
// repository can read the job log: it names no host, path or rule.
'use strict';

const fs = require('fs');
const path = require('path');

// Replaced by faber when it writes the file: the deadline in milliseconds, the
// outcome at the deadline ("allow" or "refuse"), and where the verdict lands.
const POLICY = /*FABER_POLICY*/ null;

const REASONS = new Set([
  'event',
  'fork',
  'actor',
  'author',
  'public repository',
  'no verdict',
  'unverified',
]);

let finished = false;

// Stop the job: one fixed line, then the worker, then this process.
function refuse(reason) {
  if (finished) return;
  finished = true;
  const word = REASONS.has(reason) ? reason : 'no verdict';
  try {
    process.stdout.write(`faber: this job was refused before its first step (${word})\n`);
  } catch (_) {
    // The kill below is what stops the job; the line is only its record.
  }
  try {
    process.kill(process.ppid, 'SIGKILL');
  } catch (_) {
    // A parent already gone has run nothing more.
  }
  process.exit(1);
}

// Let the job run.
function allow(line) {
  if (finished) return;
  finished = true;
  if (line) {
    try {
      process.stdout.write(`${line}\n`);
    } catch (_) {
      // Nothing to stop: the job is allowed either way.
    }
  }
  process.exit(0);
}

// Any error of its own, and any signal, stops the job.
process.on('uncaughtException', () => refuse('no verdict'));
process.on('unhandledRejection', () => refuse('no verdict'));
for (const sig of ['SIGTERM', 'SIGINT', 'SIGHUP', 'SIGQUIT', 'SIGPIPE']) {
  process.on(sig, () => refuse('no verdict'));
}

// The verdict faber wrote, or null while it has not finished writing one.
// faber writes `verdict.json` and then `ready`, in that order, in one archive:
// `ready` existing means the verdict beside it is whole.
function readVerdict(dir) {
  if (!fs.existsSync(path.join(dir, 'ready'))) return null;
  return JSON.parse(fs.readFileSync(path.join(dir, 'verdict.json'), 'utf8'));
}

// Act on a verdict. An allow counts only for this run and this attempt.
function act(v) {
  if (v === null || typeof v !== 'object') return refuse('no verdict');
  if (v.verdict === 'allow') {
    const run = String(process.env.GITHUB_RUN_ID || '');
    const attempt = String(process.env.GITHUB_RUN_ATTEMPT || '');
    if (run !== '' && attempt !== '' && String(v.run_id) === run && String(v.run_attempt) === attempt) {
      return allow('');
    }
    return refuse('no verdict');
  }
  if (v.verdict === 'refuse') return refuse(String(v.reason || ''));
  return refuse('no verdict');
}

function main() {
  if (POLICY === null || typeof POLICY !== 'object') return refuse('no verdict');
  const dir = String(POLICY.dir);
  const deadline = Date.now() + Number(POLICY.deadline_ms);
  if (!Number.isFinite(deadline)) return refuse('no verdict');
  // Whatever is there was not written for this job.
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { mode: 0o700 });
  // What faber reads in `docker top` to see this hook waiting, and so the
  // moment it may deliver: only once the directory the verdict lands in exists.
  process.title = 'faber-hook';
  const tick = () => {
    const v = readVerdict(dir);
    if (v !== null) return act(v);
    if (Date.now() >= deadline) {
      if (POLICY.outcome === 'allow') return allow('faber: no verdict by the deadline (unverified)');
      return refuse('no verdict');
    }
    setTimeout(tick, 250);
  };
  tick();
}

main();
