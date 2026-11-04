// The Overview's data shaping (ui/src/dashboard/overview.ts): the p95 wait, the tiles of a fresh
// host, in-flight rows, the activity filters and the learned caps' order. Loaded through Vite's
// SSR loader, as dashboard.test.mjs is.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let ov;
let fx;

before(async () => {
  server = await createServer({ root: ui, configFile: false, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true, hmr: false } });
  ov = await server.ssrLoadModule('/src/dashboard/overview.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
});
after(() => server?.close());

const clone = (x) => structuredClone(x);
const quiet = () => clone(fx.quiet);
const outcome = (i, patch = {}) => ({ ts_ms: fx.NOW - i * 1000, repo: 'acme/web', job_id: 100 + i, result: 'success', duration_s: 10, wait_s: 0, job_name: 'unit', ...patch });

test('p95 is the nearest rank, and null with nothing to rank', () => {
  assert.equal(ov.p95([]), null);
  assert.equal(ov.p95([7]), 7);
  assert.equal(ov.p95(Array.from({ length: 20 }, (_, i) => i + 1)), 19);
  assert.equal(ov.p95([100, 1, 2, 3, 4, 5, 6, 7, 8, 9]), 100);
  assert.equal(ov.p95([1, Number.NaN]), 1);
});

test('durations read to the second where it helps', () => {
  assert.equal(ov.fmtDur(18), '18s');
  assert.equal(ov.fmtDur(100), '1m 40s');
  assert.equal(ov.fmtDur(120), '2m');
  assert.equal(ov.fmtDur(7320), '2h 2m');
  assert.equal(ov.fmtDur(undefined), '—');
});

test('a fresh host: zero counts, no wait', () => {
  const s = quiet();
  s.counts = { provisioned: 0, completed: 0, faults: 0, refused: 0, retried: 0 };
  s.outcomes = [];
  s.uptime_s = 30;
  const tiles = ov.statTiles(s);
  assert.deepEqual(
    tiles.map((t) => [t.label, t.value]),
    [['Delivered', '0'], ["faber's own faults", '0'], ['Wait to admit', '—'], ['Refused at the gate', '0']],
  );
  assert.equal(tiles[0].note, 'since start, 30s');
  assert.equal(tiles[1].tone, undefined);
});

test('tiles: counts since start, faults toned, wait is the p95 of held outcomes', () => {
  const s = quiet();
  s.counts = { provisioned: 50, completed: 40, faults: 3, refused: 5 };
  s.outcomes = [1, 2, 3, 4, 100].map((w, i) => outcome(i, { wait_s: w }));
  const [delivered, faults, wait, refused] = ov.statTiles(s);
  assert.equal(delivered.value, '40');
  assert.equal(faults.tone, 'amber');
  assert.equal(refused.value, '5');
  assert.equal(wait.value, '1m 40s');
  assert.equal(wait.unit, undefined);
  s.outcomes = [outcome(0, { wait_s: 4 })];
  assert.deepEqual([ov.statTiles(s)[2].value, ov.statTiles(s)[2].unit], ['4', 's']);
});

test('in-flight rows: running, parked and frozen tones', () => {
  const rows = ov.inflightRows(clone(fx.full));
  assert.equal(rows.length, 3);
  const [running, parked, frozen] = rows;
  assert.equal(running.tone, undefined);
  assert.equal(running.pulse, true);
  assert.equal(running.ref, '#482');
  assert.equal(running.mib, 2048, 'the class cap in force');
  assert.equal(parked.tone, 'amber');
  assert.match(parked.step, /^Parked — waiting for memory: needs 5120 MiB/);
  // A gate is named in words, never by its key (dashboard/screens.md S11).
  const vm = ov.inflightRows({ inflight: [{ job_id: 9, repo: 'acme/web', job_name: 'e2e', admitted: false, waiting: { gate: 'vm_session' } }] })[0];
  assert.match(vm.step, /waiting for macOS VM session/);
  assert.equal(frozen.tone, 'red', 'a failing thaw is a fault');
  assert.equal(frozen.ref, 'main', 'no PR: the branch');
  assert.match(frozen.step, /^Frozen for disk, could not thaw/);
  const s = clone(fx.full);
  delete s.inflight[2].frozen.thaw_failure;
  assert.equal(ov.inflightRows(s)[2].tone, 'amber');
});

test('the rail counts parked apart and attributes the rest of the reservation', () => {
  const s = clone(fx.full);
  assert.deepEqual(ov.jobCounts(s), { admitted: 2, parked: 1 });
  const jobs = ov.budgetJobs(s);
  assert.equal(jobs[0].mib, 2048);
  assert.equal(jobs.at(-1).key, 'other');
  assert.equal(jobs.reduce((n, j) => n + j.mib, 0), s.budget.reserved_mib);
});

test('activity: counts per filter, and the filter takes the latest rows', () => {
  const s = clone(fx.full);
  const rows = ov.activityRows(s);
  const n = ov.activityCounts(rows);
  assert.equal(n.all, rows.length);
  assert.equal(n.delivered, 1);
  assert.equal(n.faults, 1);
  assert.equal(n.refused, 3, 'queued, dind and service refusals');
  assert.equal(n.retried, 1);
  assert.equal(n.delivered + n.faults + n.refused + n.retried, n.all);
  assert.deepEqual(rows.map((r) => r.ts_ms), [...rows.map((r) => r.ts_ms)].sort((a, b) => b - a), 'newest first');
  assert.deepEqual(ov.filterActivity(rows, 'faults').map((r) => r.result), ['Infra error']);
  assert.equal(ov.filterActivity(rows, 'all', 2).length, 2);
  const many = quiet();
  many.outcomes = Array.from({ length: 20 }, (_, i) => outcome(i));
  assert.equal(ov.filterActivity(ov.activityRows(many), 'all').length, ov.ACTIVITY_ROWS);
});

test('activity: results are sentence case, a cause rides the row, a fresh host has none', () => {
  const s = quiet();
  s.outcomes = [outcome(1, { result: 'timeout', cause: 'hit the lifetime cap' }), outcome(2, { result: 'weird_thing' })];
  s.retries = [];
  const [timeout, weird] = ov.activityRows(s);
  assert.equal(timeout.result, 'Timed out');
  assert.equal(timeout.tone, 'amber');
  assert.equal(timeout.cause, 'hit the lifetime cap');
  assert.equal(timeout.detailTone, 'red');
  assert.equal(weird.result, 'Weird thing');
  const empty = quiet();
  empty.outcomes = [];
  empty.retries = [];
  assert.deepEqual(ov.activityRows(empty), []);
  assert.deepEqual(ov.activityCounts([]), { all: 0, delivered: 0, faults: 0, refused: 0, retried: 0 });
});

test('learned caps: pinned first, then warned, then by cap, largest first', () => {
  const s = quiet();
  const cls = (label, status, runner, dind = 0) => ({ ...clone(fx.quiet.budget.classes[0]), key: label, label, status, pinned: status === 'pinned', runner_mib: runner, dind_mib: dind, samples: 9 });
  s.budget.classes = [cls('acme/web:test', 'learned', 2048), cls('acme/web:e2e', 'pinned', 6144), cls('acme/api:coverage', 'pinned', 8000, 2800), cls('acme/mobile:build', 'warned', 3072), cls('acme/docs:lint', 'learned', 1024)];
  assert.deepEqual(
    ov.learnedCaps(s).map((c) => [c.label, c.badge]),
    [['acme/api:coverage', 'pinned'], ['acme/web:e2e', 'pinned'], ['acme/mobile:build', 'warned'], ['acme/web:test', null], ['acme/docs:lint', null]],
  );
  s.budget.classes = [];
  assert.deepEqual(ov.learnedCaps(s), []);
});

test('host facts: rows absent when their data is', () => {
  const keys = (s) => ov.hostFacts(s).map((f) => f.key);
  assert.deepEqual(keys(quiet()), ['docker', 'disk', 'pressure', 'uptime', 'awake', 'audit']);
  const bare = quiet();
  delete bare.budget;
  delete bare.awake;
  delete bare.audit_path;
  delete bare.uptime_s;
  assert.deepEqual(keys(bare), []);
  const full = clone(fx.full);
  const facts = Object.fromEntries(ov.hostFacts(full).map((f) => [f.key, f]));
  assert.equal(facts.disk.tone, 'red');
  assert.equal(facts.pressure.value, 'elevated');
  assert.equal(facts.awake.tone, 'amber');
});

test('an alarm card: the bold head is the title, the rest the body', () => {
  const card = ov.alarmCard({ key: 'k', sev: 'warn', icon: 'alert', parts: [{ strong: 'Public repos allowed' }, ' — the gate widens.'] });
  assert.equal(card.title, 'Public repos allowed');
  assert.deepEqual(card.body, ['The gate widens.']);
  assert.equal(card.tone, 'amber');
  const plain = ov.alarmCard({ key: 'n', sev: 'bad', icon: 'alert', parts: ['Just words.'] });
  assert.equal(plain.title, 'Needs a fix');
  assert.deepEqual(plain.body, ['Just words.']);
  // Only the first character of a leading text part is raised; a mono or strong value is never touched.
  const value = ov.alarmCard({ key: 'm', sev: 'warn', icon: 'alert', parts: [{ strong: 'Stuck' }, ' — retrying each cycle. Last: ', { mono: 'x 502' }] });
  assert.deepEqual(value.body, ['Retrying each cycle. Last: ', { mono: 'x 502' }]);
  const mono = ov.alarmCard({ key: 'v', sev: 'warn', icon: 'alert', parts: [{ strong: 'Stuck' }, { mono: 'abc' }, ' later'] });
  assert.deepEqual(mono.body, [{ mono: 'abc' }, ' later']);
  assert.equal(ov.alarmCard({ key: 'i', sev: 'info', icon: 'info', parts: [{ strong: 'Hi' }] }).tone, 'neutral');
});
