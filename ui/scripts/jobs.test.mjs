// The Jobs view's data shaping (ui/src/dashboard/jobs.ts): "not measured, never zero" in every
// formatter, the job-phase split, the class medians, and the route. Loaded through Vite's SSR loader.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let j;
let fx;
let dash;

before(async () => {
  server = await createServer({ root: ui, configFile: false, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true, hmr: false } });
  j = await server.ssrLoadModule('/src/dashboard/jobs.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
  dash = await server.ssrLoadModule('/src/screens/Dashboard.tsx');
});
after(() => server?.close());

const NM = 'not measured';

test('every formatter says "not measured" for null, never zero, and a figure for 0', () => {
  const fmts = [j.fmtMs, j.fmtBytes, j.fmtCores, j.fmtShare];
  for (const f of fmts) {
    assert.equal(f(null), NM);
    assert.equal(f(undefined), NM);
    assert.notEqual(f(0), NM);
  }
  const fmt = (v) => String(v);
  assert.equal(j.fmtSpread({ median: null, p90: null, runs: 0 }, fmt), NM);
  assert.notEqual(j.fmtSpread({ median: 0, p90: 0, runs: 3 }, fmt), NM);
});

test('fmtBytes unit boundaries', () => {
  assert.equal(j.fmtBytes(999), '999 B');
  assert.equal(j.fmtBytes(1000), '1.0 kB');
  assert.equal(j.fmtBytes(12_000_000), '12 MB');
});

test('phaseSegments splits the job phase only when the steps were recorded', () => {
  const run = fx.runFootprint;
  const keys = j.phaseSegments(run.phases, run.split).map((s) => s.key);
  assert.deepEqual(keys, ['pickup', 'prepare', 'parked', 'sandbox', 'register', 'job-setup', 'job-steps', 'job-post', 'teardown']);

  const whole = j.phaseSegments(run.phases, null).map((s) => s.key);
  assert.ok(whole.includes('job'));
  assert.ok(!whole.some((k) => k.startsWith('job-')));

  const nullSetup = j.phaseSegments(run.phases, { setup: null, steps: 1, teardown: 1 }).map((s) => s.key);
  assert.ok(nullSetup.includes('job'));
  assert.ok(!nullSetup.some((k) => k.startsWith('job-')));
});

test('classSegments uses medians and carries skewed from skewed_runs', () => {
  const sp = (median) => ({ median, p90: median, runs: 4 });
  const c = {
    phases: [
      { phase: 'pickup', median: 5, p90: 9, runs: 4, skewed_runs: 2 },
      { phase: 'job', median: 100, p90: 200, runs: 4, skewed_runs: 0 },
    ],
    split: { setup: sp(10), steps: sp(80), teardown: sp(10) },
  };
  const segs = j.classSegments(c);
  assert.equal(segs.find((s) => s.key === 'pickup').ms, 5);
  assert.equal(segs.find((s) => s.key === 'pickup').skewed, true);
  assert.equal(segs.find((s) => s.key === 'job-steps').ms, 80);
  assert.equal(segs.find((s) => s.key === 'job-steps').skewed, false);
  const unsplit = j.classSegments({ ...c, split: { ...c.split, setup: { median: null, p90: null, runs: 0 } } });
  assert.ok(unsplit.some((s) => s.key === 'job'));
});

test('runSegments keeps an unmeasured teardown with ms null', () => {
  const segs = j.runSegments(fx.runFootprintPartial);
  const td = segs.find((s) => s.key === 'teardown');
  assert.equal(td.ms, null);
  assert.ok(segs.some((s) => s.key === 'job'));
});

test('viewOf routes /jobs, and anything unknown to overview', () => {
  assert.equal(dash.viewOf('/jobs'), 'jobs');
  assert.equal(dash.viewOf('/settings'), 'overview');
});
