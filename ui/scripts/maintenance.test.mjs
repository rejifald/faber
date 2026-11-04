// The Maintenance view's data shaping (ui/src/dashboard/maintenance.ts): the host's state word,
// the hold and repo-hold rows, golden standing in words, the update phase rows and the clear
// command's quoting. Loaded through Vite's SSR loader, as overview.test.mjs is.
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
  m = await server.ssrLoadModule('/src/dashboard/maintenance.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
});
after(() => server?.close());

const quiet = () => structuredClone(fx.quiet);

test('the host state word: running, paused, draining with the running count', () => {
  const s = quiet();
  assert.deepEqual([m.hostState(s).word, m.hostState(s).tone], ['Running', 'green']);
  s.paused = true;
  assert.equal(m.hostState(s).word, 'Paused');
  s.draining = true;
  const d = m.hostState(s);
  assert.equal(d.word, 'Draining', 'a drain outranks a pause');
  assert.equal(d.detail, '1 job still running');
  s.inflight = [{ job_id: 1, admitted: true }, { job_id: 2, admitted: true }, { job_id: 3, admitted: false }];
  assert.equal(m.hostState(s).detail, '2 jobs still running', 'a parked claim is not waited for');
  s.inflight = [];
  assert.equal(m.hostState(s).detail, 'No job is running');
});

test('the sidebar word: draining, paused, held, else none', () => {
  const s = quiet();
  assert.equal(m.navState(s), null);
  s.budget.hold = { reason: 'x', since_ms: fx.NOW };
  assert.equal(m.navState(s).count, 'Held');
  s.paused = true;
  assert.equal(m.navState(s).count, 'Paused');
  s.draining = true;
  assert.equal(m.navState(s).count, 'Draining');
});

test('the hold row: open, or held with its reason and since', () => {
  const s = quiet();
  assert.deepEqual([m.holdRow(s, fx.NOW).held, m.holdRow(s, fx.NOW).value], [false, 'Open']);
  s.budget.hold = { reason: 'Resizing the VM', since_ms: fx.NOW - 3600000 };
  const h = m.holdRow(s, fx.NOW);
  assert.equal(h.value, 'Held');
  assert.match(h.note, /^Resizing the VM\. Since 28 Oct, \d\d:\d\d$/);
});

test('repo-hold rows: none when nothing is held, else one per repo with reason, since and by', () => {
  const s = quiet();
  assert.deepEqual(m.repoHoldRows(s, fx.NOW), []);
  s.repo_holds = [{ repo: 'owner/demo', reason: 'a bypass ran', since_ms: fx.NOW - 60000, by: 'gate' }, { repo: 'acme/web' }];
  const rows = m.repoHoldRows(s, fx.NOW);
  assert.equal(rows.length, 2);
  assert.match(rows[0].note, /^a bypass ran, since 28 Oct, \d\d:\d\d, by gate$/);
  assert.equal(rows[1].note, '');
});

test('the clear command goes through shellWord', () => {
  assert.equal(m.clearCommand('owner/demo'), 'sudo faber repo-hold clear owner/demo');
  assert.equal(m.clearCommand("o/it's; rm -rf /"), "sudo faber repo-hold clear 'o/it'\\''s; rm -rf /'");
  const s = quiet();
  s.repo_holds = [{ repo: 'a b/c' }];
  assert.equal(m.repoHoldRows(s, fx.NOW)[0].command, "sudo faber repo-hold clear 'a b/c'");
});

test('golden standing in words', () => {
  const w = (x) => m.goldenStanding(x).word;
  assert.equal(w('verified'), 'Verified');
  assert.equal(w('unverified'), 'Not verified');
  assert.equal(w('verifying'), 'Verifying');
  assert.equal(w('failed:boot'), 'Failed: boot');
  assert.equal(w('failed'), 'Failed');
  assert.equal(w('unknown'), 'Not judged yet');
  assert.equal(w(undefined), 'Not judged yet');
  assert.equal(m.goldenStanding('failed:boot').tone, 'red');
});

test('golden rows are hidden when no golden image is configured', () => {
  const s = quiet();
  assert.equal(m.goldenRows(s).length, 1);
  s.budget.vm.golden = null;
  assert.deepEqual(m.goldenRows(s), []);
  s.budget.vm = null;
  assert.deepEqual(m.goldenRows(s), []);
  s.budget.vm = { golden: [{ image: 'g', standing: 'failed:boot', detail: 'did not boot' }] };
  assert.deepEqual(m.goldenRows(s)[0], { key: 'g', image: 'g', word: 'Failed: boot', tone: 'red', note: 'did not boot' });
});

test('update rows: idle, staged, waiting on maturity, downloading, draining, failed', () => {
  const s = quiet();
  let u = m.updateModel(s, fx.NOW);
  assert.equal(u.current, '0.4.1');
  assert.equal(u.available, null);
  assert.equal(u.phase, null);
  assert.deepEqual([u.canCheck, u.canInstall, u.canSkip], [true, false, false]);

  s.update.available = '0.4.2';
  s.update.candidate_reason = 'newest release';
  s.update.phase = { phase: 'waiting_idle', version: '0.4.2' };
  u = m.updateModel(s, fx.NOW);
  assert.deepEqual(u.available, { version: '0.4.2', why: 'newest release' });
  assert.equal(u.phase.word, 'Installs when this host goes idle', 'the panel sits under the version');
  assert.deepEqual([u.canInstall, u.canSkip], [true, true]);

  s.update.phase = { phase: 'idle' };
  s.update.install_in_secs = 7200;
  u = m.updateModel(s, fx.NOW);
  assert.equal(u.phase.word, 'Installs in 2 h');
  assert.deepEqual([u.canInstall, u.canSkip], [false, true]);

  s.update.phase = { phase: 'downloading', version: '0.4.2' };
  u = m.updateModel(s, fx.NOW);
  assert.equal(u.phase.word, 'Downloading');
  assert.deepEqual([u.canCheck, u.canSkip], [false, false]);

  s.update.phase = { phase: 'draining', version: '0.4.2' };
  assert.equal(m.updateModel(s, fx.NOW).phase.word, 'Installing');

  s.update.phase = { phase: 'failed', reason: 'the signature did not verify' };
  u = m.updateModel(s, fx.NOW);
  assert.deepEqual([u.phase.word, u.phase.tone, u.phase.note], ['Update failed', 'red', 'the signature did not verify']);
  assert.equal(u.canCheck, true);

  s.update.phase = { phase: 'disabled' };
  assert.equal(m.updateModel(s, fx.NOW).canCheck, false);
  s.update = undefined;
  assert.equal(m.updateModel(s, fx.NOW), null);
});

test('phase wording: the version-less words for a panel, the versioned words for an alarm, in every phase', () => {
  const upd = (phase, extra = {}) => ({ current: '0.4.1', available: '0.4.2', phase: { phase, version: '0.4.2', ...extra } });
  const cases = [
    ['downloading', {}, 'Downloading', 'Downloading 0.4.2'],
    ['waiting_idle', {}, 'Installs when this host goes idle', '0.4.2 installs when this host goes idle'],
    ['draining', {}, 'Installing', 'Installing 0.4.2'],
    ['deferred', { reason: 'config incompatible' }, 'Held back', '0.4.2 held back'],
  ];
  for (const [phase, extra, bare, full] of cases) {
    assert.equal(m.phaseWords(upd(phase, extra), { withVersion: false }).word, bare, phase);
    assert.equal(m.phaseWords(upd(phase, extra)).word, full, phase);
    assert.ok(!bare.includes('0.4.2'), phase);
  }
  assert.equal(m.phaseWords({ available: '0.4.2', install_in_secs: 7200 }, { withVersion: false }).word, 'Installs in 2 h');
  assert.equal(m.phaseWords({ available: '0.4.2', install_in_secs: 7200 }).word, '0.4.2 installs in 2 h');
});

test('phase notes: a missing reason is a capitalised stand-alone phrase, flagged as a fallback', () => {
  const d = m.phaseWords({ available: '0.4.2', phase: { phase: 'deferred', version: '0.4.2' } });
  assert.deepEqual([d.note, d.fallbackNote], ['Deferred', true]);
  const f = m.phaseWords({ phase: { phase: 'failed' } });
  assert.deepEqual([f.note, f.fallbackNote], ['No reason given', true]);
  const given = m.phaseWords({ phase: { phase: 'failed', reason: 'bad signature' } });
  assert.deepEqual([given.note, given.fallbackNote], ['bad signature', undefined]);
});

test('the sidebar card offers Install only where the panel does, and Skip only where the panel does', () => {
  const s = quiet();
  for (const [phase, install, skip] of [
    ['waiting_idle', true, true],
    ['idle', false, true],
    ['failed', false, true],
    ['deferred', false, true],
    ['downloading', false, false],
    ['draining', false, false],
    ['disabled', false, false],
  ]) {
    s.update = { current: '0.4.1', available: '0.4.2', phase: { phase, version: '0.4.2' } };
    const u = m.updateModel(s, fx.NOW);
    assert.deepEqual([phase, u.canInstall, u.canSkip], [phase, install, skip]);
  }
  assert.equal(m.updateModel({ ...fx.full }, fx.NOW).canInstall, false, 'the full fixture failed its update: no Install now');
});
