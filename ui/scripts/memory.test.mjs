// The Memory view's data shaping (ui/src/dashboard/memory.ts): GiB text, measured peaks, the
// bimodal rule, the learning threshold, the cold-safe suggestion, the filters, the pin request,
// and the Limits and parked-gate panels. Loaded through Vite's SSR loader, as dashboard.test.mjs is.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let m;
let fx;

before(async () => {
  server = await createServer({ root: ui, configFile: false, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true, hmr: false } });
  m = await server.ssrLoadModule('/src/dashboard/memory.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
});
after(() => server?.close());

const GIB = 1024;
const clone = (x) => structuredClone(x);

/** A class cap with the fields the shaping reads; neutral names only. */
const cap = (job, over = {}) => ({
  key: `acme/web::1::${job}`,
  label: `acme/web ${job}`,
  runner_mib: 3 * GIB,
  dind_mib: GIB / 2,
  status: 'learned',
  pinned: false,
  samples: 30,
  runner_mcpu: 1000,
  dind_mcpu: 0,
  cpu_status: 'learned',
  cpu_pinned: false,
  reserve_p75_runner_mib: 0,
  reserve_p75_dind_mib: 0,
  reserve_p75_mib: 0,
  p75_runs: 0,
  p75_trips: 0,
  peaks_runner_mib: [2900, 3000, 3100, 3000, 2950],
  peaks_dind_mib: [300, 400, 350],
  censored_runner: [],
  censored_dind: [],
  ...over,
});
const stateWith = (classes, budget = {}) => {
  const s = clone(fx.quiet);
  s.budget = { ...s.budget, margin: 1.15, classes, ...budget };
  s.inflight = [];
  s.outcomes = [];
  return s;
};
const views = (classes, budget) => m.classViews(stateWith(classes, budget), fx.NOW);
const one = (over, budget) => views([cap('build', over)], budget)[0];

const warm = Array.from({ length: 38 }, (_, i) => 2900 + ((i * 37) % 500));
const cold = Array.from({ length: 9 }, (_, i) => 8000 + ((i * 211) % 900));

test('gibText: two decimals with one trailing zero dropped', () => {
  assert.equal(m.gibText(6 * GIB), '6.0');
  assert.equal(m.gibText(0.77 * GIB), '0.77');
  assert.equal(m.gibText(1.15 * GIB), '1.15');
  assert.equal(m.gibText(1.1 * GIB), '1.1');
  assert.equal(m.gibText(0), '0.0');
});

test('measured peak is the highest runner peak plus the highest dind peak', () => {
  const c = one({});
  assert.equal(c.peakMib, 3100 + 400);
  assert.equal(m.peakText(c), '3.42');
  assert.equal(c.overCap, false);
  assert.equal(c.capMib, 3 * GIB + GIB / 2);
});

test('a class with no windows has no measured peak and no cold-safe value', () => {
  const c = one({ peaks_runner_mib: undefined, peaks_dind_mib: undefined, samples: 0 });
  assert.equal(c.peakMib, null);
  assert.equal(c.coldSafe, null);
  assert.equal(m.peakText(c), '—');
});

test('a peak above the caps in force is over the cap', () => {
  assert.equal(one({ peaks_runner_mib: [5000, 4000] }).overCap, true);
});

test('bimodal: two clusters, the high one starting at twice the low maximum or more', () => {
  const c = one({ samples: 47, peaks_runner_mib: [...warm, ...cold] });
  assert.equal(c.bimodal, true);
  assert.equal(c.coldRuns, 9);
  assert.equal(c.warmPeakMib, Math.max(...warm) + 400);
  assert.equal(c.peakMib, Math.max(...cold) + 400);
  assert.match(m.peakText(c), /^3\.\d+–9\.\d+$/);
  assert.equal(c.note, 'Bimodal, warm and cold peaks');
});

test('bimodal rule: negatives', () => {
  const peaks = (runner) => one({ peaks_runner_mib: runner }).bimodal;
  assert.equal(peaks([3000, 3100, 3200, 3300, 3400, 3500]), false, 'one cluster');
  assert.equal(peaks([3000, 3000, 3000, 3000, 3000, 3000, 9000]), false, 'a lone outlier is not a mode');
  assert.equal(peaks([3000, 3100, 3200, 4500, 4600, 4700]), false, 'a gap under twice the low maximum');
  assert.equal(peaks([100, 120, 140, 700, 720, 740]), false, 'a gap under 1 GiB');
  assert.equal(peaks([3000, 9000]), false, 'too few peaks');
  assert.equal(peaks([]), false);
});

test('still learning under 15 samples, learned at 15', () => {
  const few = one({ samples: 3 });
  assert.equal(few.learning, true);
  assert.equal(few.note, 'New class, still learning');
  assert.match(few.explanation.join(' '), /3 of 15 samples/);
  assert.equal(one({ samples: 15 }).learning, false);
  assert.equal(one({ samples: 15 }).note, null);
});

test('warned and pinned classes: status words, tones, notes and explanation', () => {
  const w = one({ status: 'warned' });
  assert.equal(w.warned, true);
  assert.equal(w.statusLabel, 'Warned');
  assert.equal(w.statusTone, 'amber');
  assert.equal(w.note, 'Auto-learn paused, needs a pin');
  const p = one({ status: 'pinned', pinned: true });
  assert.equal(p.statusLabel, 'Pinned');
  assert.equal(p.statusTone, 'accent');
  assert.match(p.explanation.join(' '), /came from a person/);
  assert.equal(one({ status: 'profiling', samples: 40 }).statusLabel, '', 'a plain learned class shows no badge');
  assert.equal(one({ status: 'learned', samples: 3 }).statusLabel, 'Learning');
  assert.equal(one({ status: 'learned', samples: 15 }).statusLabel, '');
});

test('cold-safe: highest peak times the margin, rounded up to 0.1 GiB, per container', () => {
  const c = one({ peaks_runner_mib: [8 * GIB, 3 * GIB], peaks_dind_mib: [300] });
  // 8 GiB x 1.15 = 9.2 GiB; 300 MiB x 1.15 = 0.337 GiB, up to 0.4.
  assert.equal(c.coldSafe.runnerMib, Math.round(9.2 * GIB));
  assert.equal(c.coldSafe.dindMib, Math.round(0.4 * GIB));
  assert.equal(m.ceilTenthGib(3.1 * GIB), Math.round(3.1 * GIB));
  assert.equal(m.ceilTenthGib(3.11 * GIB), Math.round(3.2 * GIB));
});

test('last runs: the latest 14 runner peaks, with their censored flags', () => {
  const runner = Array.from({ length: 20 }, (_, i) => 1000 + i);
  const c = one({ peaks_runner_mib: runner, censored_runner: runner.map((_, i) => i === 19) });
  assert.equal(c.recentPeaks.length, 14);
  assert.equal(c.recentPeaks[13], 1019);
  assert.equal(c.recentCensored[13], true);
});

test('in flight and last ran come from the daemon state, matched by repo and job', () => {
  const s = stateWith([cap('build')]);
  s.inflight = [{ job_id: 1, repo: 'acme/web', job_name: 'build', admitted: true }];
  s.outcomes = [{ ts_ms: fx.NOW - 360_000, repo: 'acme/web', job_id: 2, job_name: 'build' }];
  const c = m.classViews(s, fx.NOW)[0];
  assert.equal(c.inFlight, true);
  assert.equal(c.lastRanAgeS, 360);
  assert.equal(m.idleLine(c), 'running now');
  s.inflight = [];
  assert.equal(m.idleLine(m.classViews(s, fx.NOW)[0]), 'idle, last ran 6m ago');
  assert.equal(m.idleLine(one({})), 'idle');
});

test('filters and their counts', () => {
  const all = views([
    cap('a', { status: 'pinned', pinned: true }),
    cap('b', { status: 'pinned', pinned: true }),
    cap('c', { status: 'warned' }),
    cap('d', { peaks_runner_mib: [...warm, ...cold] }),
    cap('e'),
  ]);
  assert.deepEqual(m.filterCounts(all), { all: 5, pinned: 2, warned: 1, bimodal: 1 });
  assert.deepEqual(m.filterClasses(all, 'pinned').map((c) => c.job), ['a', 'b']);
  assert.deepEqual(m.filterClasses(all, 'warned').map((c) => c.job), ['c']);
  assert.deepEqual(m.filterClasses(all, 'bimodal').map((c) => c.job), ['d']);
  assert.equal(m.filterClasses(all, 'all').length, 5);
});

test('the class name is the repo and the job; a key with no job keeps the label', () => {
  assert.equal(one({}).name, 'acme/web:build');
  assert.equal(one({ key: 'acme/web|CI|unit' }).name, 'acme/web:unit');
  assert.equal(one({ key: 'odd', label: 'acme/web odd' }).name, 'acme/web odd');
});

test('pin request: GiB in, MiB out, and nonsense refused', () => {
  assert.deepEqual(m.pinRequest('k', '3.0', '0.4'), { class: 'k', pin: true, runner_mib: 3072, dind_mib: 410 });
  assert.deepEqual(m.pinRequest('k', ' 2 ', ''), { class: 'k', pin: true, runner_mib: 2048, dind_mib: 0 });
  assert.equal(m.pinRequest('k', '0', '1'), null);
  assert.equal(m.pinRequest('k', '', '1'), null);
  assert.equal(m.pinRequest('k', 'x', '1'), null);
  assert.equal(m.pinRequest('k', '1', '-1'), null);
  assert.deepEqual(m.unpinRequest('k'), { class: 'k', pin: false });
});

test('limits: one row each, hidden when the field is absent', () => {
  const full = m.limitRows(fx.quiet.budget);
  assert.deepEqual(full.map((r) => r.key), ['reserve', 'cpu', 'disk', 'machine', 'cotenant']);
  const byKey = Object.fromEntries(full.map((r) => [r.key, r.value]));
  assert.equal(byKey.reserve, '3.0 GiB kept for the VM, admitting on max peaks');
  assert.equal(byKey.cpu, '1.5 of 8 cores reserved');
  assert.equal(byKey.disk, '120.0 GiB free, floor 10 GiB');
  assert.equal(byKey.machine, 'keeps 8 GiB and 2 CPUs for the machine');
  assert.equal(byKey.cotenant, '2.0 GiB now, 3.32 GiB peak');
  const bare = { ...clone(fx.quiet.budget), reserve_mib: undefined, reserve: undefined, cpu: null, disk: undefined, machine: null, cotenant: null };
  assert.deepEqual(m.limitRows(bare), []);
  const some = m.limitRows({ ...bare, disk: { mode: 'on', floor_gib: 10, free_mib: 5 * GIB, fresh: true, closed: true } });
  assert.deepEqual(some.map((r) => r.key), ['disk']);
  assert.equal(some[0].tone, 'red');
});

test('limits: a held machine and a cooling reserve are amber with a note', () => {
  const b = clone(fx.quiet.budget);
  b.machine.hold = 'swap';
  b.reserve.cooling = true;
  const rows = Object.fromEntries(m.limitRows(b).map((r) => [r.key, r]));
  assert.equal(rows.machine.tone, 'amber');
  assert.match(rows.machine.note, /swap/);
  assert.equal(rows.reserve.tone, 'amber');
});

test('parked gates: longest parked first, absent counts skipped, none when none', () => {
  const rows = m.parkedGates({
    gates: {
      cpu: { parked_now: 0, parks: 6, parked_ms: 300_000, binding: 5 },
      memory: { parked_now: 1, parks: 41, parked_ms: 5_400_000, binding: 38 },
      disk: null,
    },
  });
  assert.deepEqual(rows.map((g) => g.gate), ['memory', 'cpu']);
  assert.deepEqual(rows.map((g) => g.label), ['Memory', 'CPU']);
  assert.equal(rows[0].parkedFor, '1h 30m');
  assert.equal(rows[0].arrivals, 41);
  assert.deepEqual(m.parkedGates({ gates: {} }), []);
  assert.deepEqual(m.parkedGates({}), []);
  assert.deepEqual(m.parkedGates({ gates: { memory: null } }), []);
});

test('gate names: every key the daemon sends in words, an unknown key as sentence case', () => {
  assert.equal(m.gateName('vm_session'), 'macOS VM session');
  assert.equal(m.gateName('golden_image'), 'macOS golden image');
  assert.equal(m.gateName('machine-pressure'), 'Machine memory pressure');
  assert.equal(m.gateName('some_new_gate'), 'Some new gate');
});

test('class name: repo:job from the key, the label when the key has no job; one name for every view', () => {
  assert.equal(m.classDisplayName({ key: 'acme/web::1::build', label: 'whatever' }), 'acme/web:build');
  assert.equal(m.classDisplayName({ key: 'acme/web', label: 'acme/web' }), 'acme/web');
});
