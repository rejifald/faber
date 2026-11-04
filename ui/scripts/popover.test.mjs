// The popover's data model (ui/src/popover/), held to popover/anatomy.md: groups and their pip
// order, the overflow bar's pips, the done-pip and faber-fault lingers, outcome identity, the binding gauge, and
// that the menu-bar-facing copy carries no `·`. Loaded through Vite's SSR loader, as the stories
// test loads stories.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let groups;
let fx;
let gauges;
let attention;
let format;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  groups = await server.ssrLoadModule('/src/popover/groups.ts');
  fx = await server.ssrLoadModule('/src/popover/fixtures.ts');
  gauges = await server.ssrLoadModule('/src/popover/gauges.ts');
  attention = await server.ssrLoadModule('/src/popover/attention.ts');
  format = await server.ssrLoadModule('/src/popover/format.ts');
});
after(() => server?.close());

const build = (snap, seen = new Map(), now = fx.NOW) => groups.buildGroups(snap.state, seen, now);
const byName = (list, name) => list.find((g) => g.name === name);

test('pips are ordered finished, running, frozen, waiting', () => {
  const g = byName(build(fx.busy), 'demo/PR-2741');
  assert.deepEqual(
    g.pips.map((p) => p.kind),
    ['running', 'running', 'running', 'waiting'],
  );
  const mixed = byName(build(fx.busy), 'demo/PR-2738');
  assert.deepEqual(
    mixed.pips.map((p) => p.kind),
    ['fault', 'running'],
  );
});

test('more than six jobs keep one pip each, for the overflow bar to draw', () => {
  const g = byName(build(fx.busyOverflow), 'demo/PR-2750');
  assert.equal(g.pips.length, 9);
  assert.ok(g.pips.length > groups.PIPS_MAX);
});

test('the overflow bar never draws a segment under its minimum, at any job count', async () => {
  const { barSegments, barGeometry } = await server.ssrLoadModule('/src/blocks/PipStrip.tsx');
  const SEGMENT_MIN = barGeometry.segmentMin;
  assert.equal(barSegments(Array(10).fill('running')).length, 10, 'one segment per job up to 10');
  for (let n = 7; n <= 80; n++) {
    const kinds = ['fault', ...Array(n - 3).fill('running'), 'frozen', 'waiting'];
    const segs = barSegments(kinds);
    assert.ok(segs.every((s) => s.w >= SEGMENT_MIN - 1e-9), `${n} jobs: ${segs.map((s) => s.w.toFixed(2))}`);
    const used = segs.reduce((a, s) => a + s.w, 0) + 1.5 * (segs.length - 1);
    assert.ok(Math.abs(used - 56) < 1e-6, `${n} jobs fill ${used}`);
    assert.deepEqual([...new Set(segs.map((s) => s.kind))], ['fault', 'running', 'frozen', 'waiting']);
  }
});

test('an outlined bar segment keeps a hollow of at least 1.5 px, so it never reads as filled', async () => {
  const { barSegments, barGeometry } = await server.ssrLoadModule('/src/blocks/PipStrip.tsx');
  const { segments, outline } = barGeometry;
  const outlined = ['done', 'fault', 'waiting'];
  assert.deepEqual(outlined.filter((k) => !segments[k].fill), outlined);
  for (let n = 7; n <= 80; n++) {
    for (const lone of outlined) {
      // One outlined job beside many filled ones: the run that is pinned to the floor.
      const kinds = ['frozen', ...Array(n - 2).fill('running'), lone];
      for (const s of barSegments(kinds).filter((s) => !segments[s.kind].fill)) {
        const hollow = s.w - 2 * outline;
        assert.ok(hollow >= 1.5 - 1e-9, `${n} jobs, ${s.kind}: hollow ${hollow.toFixed(2)} px`);
      }
    }
  }
});

test('an overflowing group leads with its finished pips: done, then the fault ring', () => {
  const live = Array.from({ length: 7 }, () => fx.job({ pr: 77 }));
  const withFault = { state: { inflight: live, outcomes: [fx.fault({ pr: 77 })] } };
  assert.equal(byName(build(withFault), 'demo/PR-77').pips[0].kind, 'fault');
  const withBoth = { state: { inflight: live, outcomes: [fx.fault({ pr: 77 }), fx.verdict('failure', { pr: 77, ts_ms: fx.NOW - 3000 })] } };
  assert.deepEqual(byName(build(withBoth), 'demo/PR-77').pips.slice(0, 2).map((p) => p.kind), ['done', 'fault']);
});

test('a group with only a lingering fault keeps its row, names the job, and sorts after live groups', () => {
  const list = build(fx.finished);
  const fault = byName(list, 'demo/PR-2764');
  assert.deepEqual(fault.note, { tone: 'red', text: 'runner failed for integration' });
  assert.deepEqual(fault.pips.map((p) => p.kind), ['fault']);
  assert.equal(list.at(-1).name, 'demo/PR-2764');
  const two = { state: { outcomes: [fx.fault({ pr: 3, job_name: 'a' }), fx.fault({ pr: 3, job_name: 'b' })] } };
  assert.deepEqual(byName(build(two), 'demo/PR-3').note, { tone: 'red', text: '2 runners failed' });
});

// The owner, 2026-11-01: only failures faber caused matter to faber. A job failing its own tests
// is the job's business, so the popover never reports it as a failure (ADR-0044, amended).
test('a job whose CI failed is done, not failed: no red pip, no red line, no failure word', () => {
  const g = byName(build(fx.finished), 'demo/PR-2763');
  assert.deepEqual(g.pips.map((p) => p.kind), ['done', 'done']);
  assert.equal(g.note, undefined);
  assert.doesNotMatch(g.label, /fail/);
  assert.ok(g.jobs.every((j) => j.status.kind === 'done' && j.status.text === 'done'));
  assert.match(g.jobs[0].title, /CI failure/, 'the verdict stays in the hover title');
  for (const s of [fx.busy, fx.finished, fx.folded]) {
    for (const grp of build(s)) {
      const red = grp.pips.filter((p) => p.kind === 'fault').length;
      if (!red) assert.notEqual(grp.note?.tone, 'red', `${grp.name}: a red line with no faber fault`);
    }
  }
});

test('CI success, failure and timed_out are all a done pip for 30 s', () => {
  const at = (conclusion, s) => groups.verdictPip(fx.verdict(conclusion, { ts_ms: fx.NOW - s * 1000 }), fx.NOW);
  for (const c of ['success', 'failure', 'timed_out']) {
    assert.equal(at(c, 0), 'done', c);
    assert.equal(at(c, 29.9), 'done', c);
    assert.equal(at(c, 30), null, c);
  }
});

test('a done pip lingers from when the verdict arrived, else from the run’s end', () => {
  const ended = fx.NOW - 45_000;
  // Ended 45 s ago, verdict read 20 s ago: done for another 10 s.
  const late = fx.verdict('success', { ts_ms: ended, conclusion_ts_ms: fx.NOW - 20_000 });
  assert.equal(groups.verdictPip(late, fx.NOW), 'done');
  assert.equal(groups.verdictPip(late, fx.NOW + 10_000), null);
  const g = build({ state: { outcomes: [{ ...late, pr: 8 }] } })[0];
  assert.equal(g.pips[0].ageMs, 20_000, 'the fade counts from the verdict too');
  // An older daemon: no verdict time, so the run's end.
  const old = { ...late, conclusion_ts_ms: undefined };
  assert.equal(groups.verdictPip(old, fx.NOW), null);
  assert.equal(groups.verdictPip({ ...old, ts_ms: fx.NOW - 29_000 }, fx.NOW), 'done');
});

test('cancelled, skipped, neutral and an unread conclusion draw no pip', () => {
  for (const conclusion of ['cancelled', 'skipped', 'neutral', 'action_required', null, undefined]) {
    const snap = { state: { outcomes: [fx.verdict(conclusion, { pr: 9 })] } };
    assert.deepEqual(build(snap), [], String(conclusion));
  }
  assert.equal(byName(build(fx.finished), 'demo/PR-2765'), undefined);
  assert.equal(byName(build(fx.finished), 'demo/PR-2766'), undefined);
});

test('an older daemon with no conclusion field draws no verdict pips, and the fault ring still shows', () => {
  const NEW = ['conclusion', 'pr', 'branch', 'workflow', 'job_name', 'event'];
  const old = (o) => Object.fromEntries(Object.entries(o).filter(([k]) => !NEW.includes(k)));
  const outcomes = [fx.verdict('success'), fx.fault()].map(old);
  const list = build({ state: { outcomes } });
  assert.deepEqual(list.flatMap((g) => g.pips.map((p) => p.kind)), ['fault']);
});

test('a runner faber lost is a fault ring even when CI then concluded failure', () => {
  const o = fx.fault({ result: 'failure', conclusion: 'failure', pr: 5 });
  assert.equal(groups.finishedPip(o, fx.NOW), 'fault');
});

test('finished pips come first, done then fault, and the label says them in words', () => {
  const g = byName(build(fx.finished), 'demo/PR-2760');
  assert.deepEqual(g.pips.map((p) => p.kind), ['done', 'done', 'running', 'running']);
  assert.match(g.label, /2 running, 2 done/);
  assert.deepEqual(
    g.jobs.slice(0, 2).map((j) => [j.name, j.status.kind, j.status.text]),
    [
      ['lint', 'done', 'done'],
      ['types', 'done', 'done'],
    ],
  );
  assert.equal(g.note, undefined, 'a live group carries no failure line');
  const mixed = { state: { outcomes: [fx.fault({ pr: 6 }), fx.verdict('success', { pr: 6, ts_ms: fx.NOW - 5000 }), fx.verdict('failure', { pr: 6, ts_ms: fx.NOW - 3000 })] } };
  assert.deepEqual(byName(build(mixed), 'demo/PR-6').pips.map((p) => p.kind), ['done', 'done', 'fault']);
});

test('a done pip carries how long ago its run ended, for its fade; others carry nothing', () => {
  const g = byName(build(fx.finished), 'demo/PR-2762');
  assert.deepEqual(g.pips, [{ kind: 'done', key: g.pips[0].key, ageMs: 25_000 }]);
  assert.equal(byName(build(fx.finished), 'demo/PR-2764').pips[0].ageMs, undefined);
});

test('a group running here comes before older groups that only wait', () => {
  // The menu-bar count is jobs admitted here; the shared queue is older and must not bury them.
  const snap = {
    state: {
      inflight: [
        fx.job({ job_id: 1, pr: 3810, admitted: false, elapsed_s: 18_000 }),
        fx.job({ job_id: 2, pr: 445, repo: 'owner/faber', elapsed_s: 11_000 }),
        fx.job({ job_id: 3, pr: 3809, admitted: false, elapsed_s: 17_000 }),
        fx.job({ job_id: 4, pr: 3700, elapsed_s: 900, frozen: { since_ms: fx.NOW, reason: 'disk', detail: '' } }),
      ],
    },
  };
  assert.deepEqual(
    build(snap).map((g) => g.name),
    ['faber/PR-445', 'demo/PR-3700', 'demo/PR-3810', 'demo/PR-3809'],
  );
});

test('finished-only groups follow the live ones, newest outcome first, aged from it', () => {
  const names = build(fx.finished).map((g) => g.name);
  assert.deepEqual(names, ['demo/PR-2760', 'demo/PR-2761', 'demo/PR-2762', 'demo/PR-2763', 'demo/PR-2764']);
  assert.equal(byName(build(fx.finished), 'demo/PR-2761').age, '8s');
});

test('folded groups go worst first, stably, and the tally counts each group by its worst pip', () => {
  const g = (name, ...kinds) => ({ name, pips: kinds.map((kind, i) => ({ kind, key: `${name}${i}` })) });
  const list = [g('a', 'done'), g('b', 'running', 'done'), g('c', 'waiting'), g('d', 'running'), g('e', 'running', 'fault'), g('f', 'frozen')];
  assert.deepEqual(
    groups.worstFirst(list).map((x) => x.name),
    ['e', 'f', 'c', 'b', 'd', 'a'],
    'runner failed, frozen, waiting, then running in their given order, then done',
  );
  assert.deepEqual(list.map((x) => x.name), ['a', 'b', 'c', 'd', 'e', 'f'], 'the input is not reordered');
  assert.deepEqual(groups.pipTally(list), [
    { kind: 'fault', count: 1 },
    { kind: 'frozen', count: 1 },
    { kind: 'waiting', count: 1 },
    { kind: 'running', count: 2 },
    { kind: 'done', count: 1 },
  ]);
  assert.deepEqual(groups.pipTally([]), []);
  assert.deepEqual(groups.URGENCY.slice(0, 2), ['fault', 'frozen']);
});

test('the attention tally counts the severities present, worst first', () => {
  const rows = [{ sev: 'info' }, { sev: 'warn' }, { sev: 'info' }];
  assert.deepEqual(attention.severityTally(rows), [
    { sev: 'warn', count: 1 },
    { sev: 'info', count: 2 },
  ]);
  assert.deepEqual(attention.severityTally([]), []);
});

test('a fault stops showing after 10 minutes', () => {
  const later = fx.NOW + groups.FAULT_LINGER_MS;
  assert.equal(byName(build(fx.finished, new Map(), later), 'demo/PR-2764'), undefined);
});

test('a success, a reclaim and a cancel draw no pip', () => {
  for (const result of ['success', 'reclaimed', 'cancelled']) {
    const snap = { state: { outcomes: [fx.fault({ result, pr: 9 })] } };
    assert.deepEqual(build(snap), [], result);
  }
});

test('an outcome without identity borrows the live job of the same id, else the label, never a PR', () => {
  const live = fx.job({ pr: 4242, job_name: 'api e2e' });
  const bare = { ts_ms: fx.NOW - 1000, repo: 'owner/demo', job_id: live.job_id, label: ['owner/demo', 'CI', 'api e2e'].join(groups.DAEMON_LABEL_SEP), result: 'failure' };
  const seen = new Map([[live.job_id, live]]);
  assert.equal(groups.outcomeIdentity(bare, seen).pr, 4242);
  const other = groups.outcomeIdentity({ ...bare, job_id: live.job_id + 1 }, seen);
  assert.equal(other.pr, undefined);
  assert.equal(other.workflow, 'CI');
  assert.equal(other.jobName, 'api e2e');
});

test('an all-waiting group has no note: only its open job lines say why', () => {
  const at = (gate) => {
    const state = structuredClone(fx.waiting.state);
    for (const j of state.inflight) if (!j.admitted) j.waiting = { ...j.waiting, gate };
    return byName(groups.buildGroups(state, new Map(), fx.NOW), 'site');
  };
  for (const [gate, text] of [['memory', 'waits for memory'], ['github', 'waits for GitHub'], ['image-pull', 'waits for its image'], ['docker', 'waits for Docker']]) {
    const g = at(gate);
    assert.equal(g.note, undefined, gate);
    assert.ok(g.jobs.length > 0 && g.jobs.every((j) => j.status.kind === 'waiting' && j.status.text === text), gate);
  }
});

test('the binding gauge is amber while claims are parked, the accent otherwise', () => {
  const parked = gauges.bindingGate(fx.waiting.state);
  assert.deepEqual(parked, { gate: 'memory', parked: true });
  const mem = gauges.gaugeModels(fx.waiting.state.budget, parked.gate, parked.parked).find((r) => r.key === 'mem');
  assert.equal(mem.bind, 'warn');
  assert.equal(mem.caption, 'Memory', 'the binding gate keeps its name');
  assert.match(mem.aria, /, binds$/);
  const idle = gauges.bindingGate(fx.idle.state);
  assert.deepEqual(idle, { gate: 'memory', parked: false });
});

test('no row or group text joins facts with a middle dot', () => {
  const texts = [];
  for (const snap of [fx.busy, fx.busyOverflow, fx.waiting, fx.pausedFrozen, fx.attention, fx.attentionActions, fx.mirrorDown, fx.mirrorPassing, fx.mirrorImageMissing, fx.macosHeld, fx.oomHeld]) {
    for (const g of build(snap)) texts.push(g.name, g.workflow ?? '', g.label, g.note?.text ?? '', ...g.jobs.map((j) => j.name + j.status.text));
    for (const r of attention.daemonRows(snap.state, fx.NOW)) texts.push(JSON.stringify(r.parts));
  }
  const dotted = texts.filter((t) => t.includes('·'));
  assert.deepEqual(dotted, []);
});

test('an older or partial daemon state draws without throwing', () => {
  const partial = {
    budget: {
      gates: { memory: null, cpu: {} },
      cpu: null,
      disk: { free_mib: 1024 },
      headroom: null,
      machine: { hold: null, encroaches: null },
      vm: { vms: null, images: null },
      reserve: { cooling: true, last_trip: null },
    },
    update: { supervisor_restart: { detail: 'restarted' }, phase: null, self_check: { status: 'refused' } },
    inflight: [{ job_id: 1, admitted: false }, { job_id: 2, admitted: true, step: null, frozen: {} }],
    outcomes: [{ ts_ms: fx.NOW - 1000, job_id: 3, result: 'failure' }],
    peers: [{ instance: 'c3d4', runners: null }],
    repo_holds: [{ repo: 'owner/demo' }],
    refused_queued: [{}],
    trust_changes: [{ ts_ms: fx.NOW, key: 'k', sentence: 'allowed forks', via: 'session' }],
  };
  const rows = [...attention.daemonRows(partial, fx.NOW), ...attention.updateRows(partial.update)];
  assert.ok(rows.some((r) => r.key === 'upd-sup'));
  const gate = gauges.bindingGate(partial);
  assert.ok(gauges.gaugeModels(partial.budget, gate.gate, gate.parked).length > 0);
  assert.ok(build({ state: partial }).length > 0);
  // And an empty state draws nothing at all.
  assert.deepEqual(attention.daemonRows({}, fx.NOW), []);
  assert.deepEqual(gauges.gaugeModels({}, null, false), []);
  assert.deepEqual(build({ state: {} }), []);
});

test('a mirror outage counts the jobs it affected and never names them; ids and commands are mono', () => {
  const [down] = attention.daemonRows(fx.mirrorDown.state, fx.NOW);
  assert.equal(down.key, 'mirror-down');
  assert.equal(down.sev, 'bad');
  assert.deepEqual(down.parts[0], { strong: 'Mirror down, 3 jobs affected' });
  assert.ok(!JSON.stringify(down.parts).includes('faber-1001'), 'runner names belong to the dashboard');
  assert.ok(down.parts.some((p) => p.mono === 'health probe: exited 1'));

  const [image] = attention.daemonRows(fx.mirrorImageMissing.state, fx.NOW);
  assert.deepEqual(image.parts[0], { strong: 'Mirror image missing' });
  assert.ok(image.parts.some((p) => p.mono === 'faber-mirror:0.3.12'));
  assert.ok(image.parts.some((p) => p.mono === 'deploy/build-mirror-image.sh'));

  const [passing] = attention.daemonRows(fx.mirrorPassing.state, fx.NOW);
  assert.equal(passing.sev, 'warn');
  assert.ok(passing.parts.some((p) => typeof p === 'string' && p.includes('storing nothing')));
});

test('a class held at the threshold while it OOMs is one red row, naming the worst', () => {
  const rows = attention.daemonRows(fx.oomHeld.state, fx.NOW);
  const oom = rows.find((r) => r.key === 'oom-held');
  assert.equal(oom.sev, 'bad');
  assert.deepEqual(oom.parts[0], { strong: 'OOM-killed on 5 of its last 10 runs' });
  assert.deepEqual(oom.parts.slice(2, 7), ['owner/demo', { sep: true }, 'CI', { sep: true }, 'test']);
  // Held and Pin are both runner + dind: 8704 → 10752 MiB.
  assert.equal(oom.parts[7], ' (+1 more) — held at 8.5 GiB.');
  // Pin sets the daemon's figures for the named class.
  assert.deepEqual(oom.actions, [
    {
      label: 'Pin at 10.5 GiB',
      cmd: 'set_pin',
      args: { class: 'owner/demo::1::ci', runnerMib: 10240, dindMib: 512, pin: true },
      confirm: {
        question: 'Pin at 10.5 GiB?',
        consequence:
          'Each of its jobs reserves 10.5 GiB, more than faber grants a class on its own, and faber stops learning its size. Unpin it in the panel.',
        label: 'Pin',
      },
    },
  ]);
  // An older daemon without the figures: the ask stays, the button goes.
  const old = { budget: { oom_held: [{ key: 'k', label: 'k', cap_mib: 6144 }] } };
  const oldRow = attention.daemonRows(old, fx.NOW).find((r) => r.key === 'oom-held');
  assert.equal(oldRow.actions, undefined);
  assert.match(oldRow.parts.at(-1), /held at 6\.0 GiB, .* Pin it higher, or make the job lighter\.$/);
  assert.equal(rows.filter((r) => r.key === 'oom-held').length, 1);
  // None held, or an older daemon without the field: no row.
  assert.ok(!attention.daemonRows({ budget: { oom_held: [] } }, fx.NOW).some((r) => r.key === 'oom-held'));
  assert.ok(!attention.daemonRows({ budget: { oom_held: null } }, fx.NOW).some((r) => r.key === 'oom-held'));
});

test('a macOS gate only a person can open is a row, counting the jobs it holds', () => {
  const rows = attention.daemonRows(fx.macosHeld.state, fx.NOW);
  const session = rows.find((r) => r.key === 'macos-refusal:vm_session');
  assert.equal(session.sev, 'bad');
  assert.deepEqual(session.parts[0], { strong: 'No macOS VM session' });
  assert.match(session.parts[1], /^ — 2 macOS jobs wait\. Unlock the disk as _faber_mac/);
  const golden = rows.find((r) => r.key === 'golden');
  assert.equal(golden.sev, 'warn', 'nothing waits on it yet');
  assert.deepEqual(golden.parts[0], { strong: 'Golden image not verified: macos-golden' });

  const vm = fx.macosHeld.state.budget.vm;
  const at = (over, gates = {}) => attention.daemonRows({ budget: { gates, vm: { ...vm, golden: [], ...over } } }, fx.NOW).map((r) => r.key);
  // No job waiting yet: still said, as a warning, since nothing clears it but a person.
  assert.deepEqual(at({}), ['macos-refusal:vm_session']);
  // The first seconds after a start, before the session is probed, are quiet.
  assert.deepEqual(at({ session: 'unprobed' }), []);
  // A gate that clears on its own shows only while it holds a job.
  const mem = { refusal: { gate: 'vm_memory', detail: 'a macOS VM needs 8 GiB' }, session: 'up' };
  assert.deepEqual(at(mem), []);
  assert.deepEqual(at(mem, { vm_memory: { parked_now: 1 } }), ['macos-refusal:vm_memory']);
  // The wall and the cap keep their own rows, and Docker jobs parked on the shared disk gate are
  // never counted as macOS jobs.
  assert.deepEqual(at({ refusal: { gate: 'macos_cap', detail: '' }, session: 'up' }, { macos_cap: { parked_now: 1 } }), []);
  assert.deepEqual(at({ refusal: { gate: 'disk', detail: 'low' }, session: 'up' }, { disk: { parked_now: 5 } }), []);
  // A golden image not judged yet is a row once a job waits on it, not before.
  const unjudged = { session: 'up', refusal: null, golden: [{ image: 'macos-golden', standing: 'unknown', detail: 'has not been judged' }] };
  assert.deepEqual(at(unjudged), []);
  assert.deepEqual(at(unjudged, { golden_image: { parked_now: 1 } }), ['golden']);
});

test('a byte count picks its unit, so a small one never reads 0.0 GiB', () => {
  assert.equal(format.bytes(0), '0 B');
  assert.equal(format.bytes(undefined), '0 B');
  assert.equal(format.bytes(1023), '1023 B');
  assert.equal(format.bytes(812 * 1024), '812.0 KiB');
  assert.equal(format.bytes(1024 * 1024), '1.0 MiB');
  assert.equal(format.bytes(1024 * 1024 - 1), '1.0 MiB', 'never 1024.0 KiB');
  assert.equal(format.bytes(3.1 * 2 ** 30), '3.1 GiB');
  assert.equal(format.bytes(20 * 2 ** 40), '20.0 TiB');
});

test('memorySegments splits the engine memory into reserve, jobs, free and headroom', async () => {
  const limits = await server.ssrLoadModule('/src/popover/limits.ts');
  const s = limits.memorySegments({ budget_mib: 16384, reserve_mib: 2048, reserved_mib: 6144, headroom_mib: 1024 });
  assert.deepEqual([s.totalMib, s.reserveMib, s.jobsMib, s.freeMib, s.headroomMib], [18432, 2048, 6144, 9216, 1024]);
  assert.deepEqual(s.display, { total: '18.0', reserve: '2.0', jobs: '6.0', free: '9.0', headroom: '1.0' });
  const idle = limits.memorySegments({ budget_mib: 16384, reserve_mib: 2048, reserved_mib: 0, headroom_mib: 1024 });
  assert.equal(idle.jobsMib, 0);
  assert.equal(idle.display.jobs, '0.0');
  const over = limits.memorySegments({ budget_mib: 8000, reserve_mib: 2048, reserved_mib: 7500, headroom_mib: 1024 });
  assert.equal(over.freeMib, 0);
  assert.equal(limits.memorySegments({ budget_mib: 8000, reserved_mib: 10 }), null);
  assert.equal(limits.memorySegments({}), null);
  assert.equal(limits.memorySegments(undefined), null);
});

test('stepValue steps, clamps and does not drift on fractions', async () => {
  const { stepValue } = await server.ssrLoadModule('/src/kit/Stepper.tsx');
  assert.equal(stepValue(2048, 512, 0), 2560);
  assert.equal(stepValue(0, -512, 0), 0);
  assert.equal(stepValue(5, 1, 1, 5), 5);
  assert.equal(stepValue(1.2, 0.05, 1), 1.25);
  assert.equal(stepValue(1.1, -0.05, 1), 1.05);
});

test('knob display: GiB with one decimal, the margin as a multiple, counts plain', async () => {
  const l = await server.ssrLoadModule('/src/popover/limits.ts');
  assert.equal(l.showKnob('reserve', 2048), '2.0 GiB');
  assert.equal(l.showKnob('headroom', 2560), '2.5 GiB');
  assert.equal(l.showKnob('margin', 1.2), '1.2×');
  assert.equal(l.showKnob('margin', 1.25), '1.25×');
  assert.equal(l.showKnob('native', 2), '2');
  assert.deepEqual(l.KNOBS.map((k) => [k.key, k.step, k.min]), [
    ['ceiling', 1, 1],
    ['reserve', 512, 0],
    ['headroom', 512, 1],
    ['margin', 0.05, 1],
    ['native', 1, 1],
  ]);
});

test('limitsAvailable needs the four knobs an older daemon omits', async () => {
  const l = await server.ssrLoadModule('/src/popover/limits.ts');
  assert.equal(l.limitsAvailable(undefined), false);
  assert.equal(l.limitsAvailable({ budget_mib: 1 }), false);
  assert.equal(l.limitsAvailable({ reserve_mib: 0, headroom_mib: 1, margin: 1.2, native_concurrency: 2 }), true);
});

test('the hard ceiling: auto shows the resolved number, off reads Off, a number offers Use auto', async () => {
  const { ceilingView } = await server.ssrLoadModule('/src/popover/limits.ts');
  const auto = ceilingView({ hard_ceiling: 6, hard_ceiling_setting: 'auto' }, null);
  assert.deepEqual([auto.mode, auto.value, auto.display, auto.hint, auto.canUseAuto], ['auto', 6, '6', 'Auto, faber’s pick', false]);
  // The first press makes it explicit.
  const pressed = ceilingView({ hard_ceiling: 6, hard_ceiling_setting: 'auto' }, 7);
  assert.deepEqual([pressed.mode, pressed.display, pressed.canUseAuto], ['explicit', '7', true]);
  const off = ceilingView({ hard_ceiling: null, hard_ceiling_setting: 'off' }, null);
  assert.deepEqual([off.mode, off.display, off.hint, off.fromOff], ['off', 'Off', 'Budget only', 1]);
  assert.equal(ceilingView({ hard_ceiling: 5, hard_ceiling_setting: 'off' }, null).fromOff, 5);
  const num = ceilingView({ hard_ceiling: 4, hard_ceiling_setting: 4 }, null);
  assert.deepEqual([num.mode, num.value, num.hint, num.canUseAuto], ['explicit', 4, 'Most jobs at once', true]);
});

test('the CPU warning only when CPU binds and the explicit ceiling is above inflight plus headroom jobs', async () => {
  const { ceilingView, cpuWarning } = await server.ssrLoadModule('/src/popover/limits.ts');
  const b = { hard_ceiling: 8, hard_ceiling_setting: 8, docker_inflight: 2, headroom: { jobs: 2, binds: 'cpu' } };
  assert.equal(ceilingView(b, null).warning, 'Above what CPU allows. CPU binds at 4.');
  assert.equal(cpuWarning(b, 4), undefined);
  assert.equal(cpuWarning({ ...b, headroom: { jobs: 2, binds: 'memory' } }, 8), undefined);
  assert.equal(cpuWarning({ ...b, headroom: null }, 8), undefined);
});

test('a row goes applying, applied, idle; an error keeps its message; an older response is ignored', async () => {
  const { rowReducer, IDLE_ROW, cut } = await server.ssrLoadModule('/src/popover/limits.ts');
  let r = rowReducer(IDLE_ROW, { type: 'press', seq: 1, value: 2560, arg: 2560 });
  assert.deepEqual([r.status, r.shown], ['applying', 2560]);
  r = rowReducer(r, { type: 'press', seq: 2, value: 3072, arg: 3072 });
  assert.equal(rowReducer(r, { type: 'ok', seq: 1 }).status, 'applying');
  assert.equal(rowReducer(r, { type: 'fail', seq: 1, message: 'x' }).status, 'applying');
  const ok = rowReducer(r, { type: 'ok', seq: 2 });
  assert.equal(ok.status, 'applied');
  assert.equal(rowReducer(ok, { type: 'clear', seq: 1 }).status, 'applied');
  assert.deepEqual(rowReducer(ok, { type: 'clear', seq: 2 }), { ...IDLE_ROW, seq: 2 });
  const bad = rowReducer(r, { type: 'fail', seq: 2, message: 'daemon unreachable' });
  assert.deepEqual([bad.status, bad.message, bad.shown, bad.arg], ['error', 'daemon unreachable', 3072, 3072]);
  assert.equal(rowReducer(bad, { type: 'clear', seq: 2 }).status, 'error');
  assert.equal(cut('a'.repeat(80)).length, 80);
  assert.equal(cut('a'.repeat(81)), `${'a'.repeat(80)}…`);
});

test('the debouncer sends one request per settle, each key on its own', async () => {
  const { debouncer } = await server.ssrLoadModule('/src/popover/limits.ts');
  const timers = new Map();
  let id = 0;
  const sched = { set: (fn) => (timers.set(++id, fn), id), clear: (i) => timers.delete(i) };
  const sent = [];
  const d = debouncer((k, v) => sent.push([k, v]), 400, sched);
  for (let i = 1; i <= 10; i++) d.push('reserve', i);
  d.push('margin', 1.25);
  assert.equal(timers.size, 2);
  for (const [i, fn] of [...timers]) {
    timers.delete(i);
    fn();
  }
  assert.deepEqual(sent, [['reserve', 10], ['margin', 1.25]]);
  d.push('native', 3);
  d.flush('native', 4);
  assert.deepEqual(sent.at(-1), ['native', 4]);
  assert.equal(timers.size, 0);
  d.push('reserve', 11);
  d.flushAll();
  assert.deepEqual(sent.at(-1), ['reserve', 11]);
});

test('an open gate lists only the lines whose fields the daemon sent', () => {
  const facts = (key, b, gate = null, parked = false) =>
    gauges.gateFacts(gauges.gaugeModels(b, gate, parked).find((g) => g.key === key), b).map((f) => f.label);
  const full = fx.budget({ docker_inflight: 3, reserved_mib: 5632, hard_ceiling_setting: 'auto', reserve: { in_force: 'p75' } });
  assert.deepEqual(facts('slots', full), ['Running', 'Free slots', 'Ceiling']);
  assert.deepEqual(facts('mem', full), ['Reserved by jobs', 'Free', 'Next job reserves', 'Room for', 'Reserve sized by']);
  assert.deepEqual(facts('cpu', full), ['Reserved by jobs', 'Free', 'Next job reserves']);
  assert.deepEqual(facts('disk', full), ['Free on the host', 'Held by running jobs', 'Floor kept free', 'Spare above the floor']);

  // An older daemon: no headroom, no ceiling setting, no reserve mode, no held disk.
  const old = fx.budget({ headroom: null, docker_inflight: 1, disk: { mode: 'on', free_mib: 100 * 1024, floor_gib: 50 } });
  assert.deepEqual(facts('slots', old), ['Running', 'Free slots']);
  assert.deepEqual(facts('mem', old), ['Reserved by jobs', 'Free']);
  assert.deepEqual(facts('cpu', old), ['Reserved by jobs', 'Free']);
  assert.deepEqual(facts('disk', old), ['Free on the host', 'Floor kept free', 'Spare above the floor']);
  assert.deepEqual(facts('disk', fx.budget({ disk: { mode: 'off', free_mib: 400 * 1024 } })), ['Free on the host', 'Floor']);
});

test('"Reserve sized by" shows only for the 75th percentile, not the default largest job', () => {
  const sizedBy = (in_force) => {
    const b = fx.budget({ reserve: { in_force } });
    return gauges.gateFacts(gauges.gaugeModels(b, null, false).find((g) => g.key === 'mem'), b).find((f) => f.label === 'Reserve sized by');
  };
  assert.deepEqual(sizedBy('p75'), { label: 'Reserve sized by', value: '75th percentile' });
  assert.equal(sizedBy('max'), undefined);
});

test('a field the daemon sent as 0 still shows its line', () => {
  const b = fx.budget({
    headroom: { jobs: 2, binds: 'memory', job_mib: 0, job_mcpu: 0 },
    cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 0 },
    disk: { mode: 'on', free_mib: 180 * 1024, held_mib: 0, floor_gib: 50 },
  });
  const line = (key, label) =>
    gauges.gateFacts(gauges.gaugeModels(b, null, false).find((g) => g.key === key), b).find((f) => f.label === label);
  assert.deepEqual(line('mem', 'Next job reserves'), { label: 'Next job reserves', value: '0.0 GiB' });
  assert.deepEqual(line('cpu', 'Next job reserves'), { label: 'Next job reserves', value: '0.0 cores' });
  assert.deepEqual(line('disk', 'Held by running jobs'), { label: 'Held by running jobs', value: '0 GiB' });
});

test('a gate with claims parked ends on the amber wait line, and memory drops "Room for"', () => {
  const b = fx.budget({ docker_inflight: 2, reserved_mib: 5632, gates: { memory: { parked_now: 3 } } });
  const mem = gauges.gaugeModels(b, 'memory', true).find((g) => g.key === 'mem');
  const lines = gauges.gateFacts(mem, b);
  assert.ok(!lines.some((f) => f.label === 'Room for'), 'room for more would contradict the wait');
  assert.deepEqual(lines.at(-1), { label: '3 jobs wait for memory', waits: true });
  const one = fx.budget({ gates: { ceiling: { parked_now: 1 } } });
  const slots = gauges.gaugeModels(one, 'ceiling', true).find((g) => g.key === 'slots');
  assert.deepEqual(gauges.gateFacts(slots, one).at(-1), { label: '1 job waits for a slot', waits: true });
});

test('a disk gate from a daemon that sends no mode leaves the floor out, and its figures are whole GiB', () => {
  const b = fx.budget({ disk: { free_mib: 180 * 1024 + 300, held_mib: 10 * 1024 } });
  const disk = gauges.gaugeModels(b, null, false).find((g) => g.key === 'disk');
  assert.deepEqual(gauges.gateFacts(disk, b), [
    { label: 'Free on the host', value: '180 GiB' },
    { label: 'Held by running jobs', value: '10 GiB' },
  ]);
});

test('gib is MiB over 1024, whole without a decimal and one decimal otherwise', () => {
  assert.equal(format.gib(8192), '8 GiB');
  assert.equal(format.gib(12 * 1024), '12 GiB');
  assert.equal(format.gib(12.5 * 1024), '12.5 GiB');
  assert.equal(format.gib(1536), '1.5 GiB');
  assert.equal(format.gib(0), '0 GiB');
});

test('vmSection: doing, status and tone per state, stopped hidden, Linux first then macOS by slot', async () => {
  const { vmSection } = await server.ssrLoadModule('/src/popover/vms.ts');
  const t0 = fx.NOW;
  const s0 = Math.floor(t0 / 1000);
  const vm = (kind, slot, state, over = {}) => ({ kind, slot, state, started_unix: s0 - 720, cpus: 4, memory_mib: 8192, job: null, ...over });
  const b = (vms, over = {}) => ({ docker_inflight: 3, disk: { vm_free_mib: 40 * 1024 }, vm: { listed_age_s: 5, vms }, ...over });
  const m = vmSection(
    b([
      vm('macos', 1, 'booting', { started_unix: s0 - 40 }),
      vm('macos', 0, 'in_service', { job: { repo: 'owner/demo', job_name: 'release' } }),
      vm('macos', 2, 'stopped'),
      vm('linux', 0, 'in_service', { cpus: 6, memory_mib: 16 * 1024, started_unix: s0 - 7200 }),
    ]),
    t0,
  );
  assert.deepEqual(
    m.rows.map((r) => [r.kind, r.slot, r.doing, r.status, r.tone]),
    [
      ['Linux', 0, '3 jobs', 'serving', 'green'],
      ['macOS', 0, 'release', 'serving', 'green'],
      ['macOS', 1, '', 'settling', 'amber'],
    ],
  );
  assert.deepEqual(m.rows[0].facts, [
    { label: 'CPU', value: '6 vCPU' },
    { label: 'Memory', value: '16 GiB' },
    { label: 'Disk free', value: '40 GiB' },
  ]);
  assert.deepEqual(m.rows[1].facts.map((f) => f.label), ['CPU', 'Memory'], 'free disk is the Linux VM only');
  assert.deepEqual([m.rows[0].age, m.rows[1].age, m.rows[2].age], ['2h', '12m', '40s']);
  assert.equal(m.rows[0].label, 'Linux VM, serving 3 jobs, 6 vCPU, 16 GiB, 40 GiB disk free, up 2 hours');
  assert.equal(m.rows[2].label, 'macOS VM, booting, 4 vCPU, 8 GiB, up 40 seconds');
  assert.deepEqual(m.rows[2].facts[0], { label: 'State', value: 'booting', word: true }, 'the icon cannot say booting from proving the wall');
  assert.equal(m.asOf, null);
  assert.equal(m.dim, false);

  const one = (state, over, budget = {}) => vmSection(b([vm('linux', 0, state, over)], budget), t0).rows[0];
  const idle = one('in_service', {}, { docker_inflight: 0 });
  assert.deepEqual([idle.doing, idle.status, idle.tone], ['', 'idle', 'neutral']);
  assert.equal(one('job_finished', {}, { docker_inflight: 1 }).doing, '1 job');
  for (const [state, doing, status] of [
    ['starting', 'booting', 'settling'],
    ['ready', 'booting', 'settling'],
    ['probe_sent', 'proving the wall', 'settling'],
    ['stopping', 'stopping', 'stopping'],
  ]) {
    const r = one(state);
    assert.deepEqual([r.doing, r.status, r.tone, r.facts[0]], ['', status, 'amber', { label: 'State', value: doing, word: true }], state);
  }
  const un = one('unmanaged');
  assert.deepEqual([un.doing, un.status, un.tone, un.facts[0].value], ['', 'unmanaged', 'neutral', 'unmanaged']);
  // A fork's job has no name: the repo stands in. No job: no text (no bare default state).
  const fork = vmSection(b([vm('macos', 0, 'in_service', { job: { repo: 'owner/demo', job_name: null } })]), t0).rows[0];
  assert.equal(fork.doing, 'owner/demo');
  assert.equal(vmSection(b([vm('macos', 0, 'in_service')]), t0).rows[0].doing, '');
});

test('peerRows: a running pip per busy runner, an idle ring per idle one, none for offline or stale', async () => {
  const { peerRows } = await server.ssrLoadModule('/src/popover/peers.ts');
  const peer = (agoS, over) => peerRows([{ instance: 'c3d4', last_seen_ms: fx.NOW - agoS * 1000, ...over }], fx.NOW)[0];
  // owner/a: 2 busy, 1 idle, 1 offline; owner/b: 1 idle.
  const counts = { runners: { 'owner/a': 4, 'owner/b': 1 }, busy: { 'owner/a': 2, 'owner/b': 0 }, idle: { 'owner/a': 1, 'owner/b': 1 } };
  const fresh = peer(40, counts);
  assert.deepEqual(fresh.pips.map((p) => p.kind), ['running', 'running', 'idle', 'idle']);
  assert.equal(new Set(fresh.pips.map((p) => p.key)).size, 4, 'stable, distinct keys');
  assert.equal(fresh.label, 'peer c3d4, 5 runners, 2 busy, seen 40 seconds ago');
  // Stale: nobody knows which runners still exist, so no pips; the label keeps the last count.
  const stale = peer(7 * 60, counts);
  assert.deepEqual([stale.pips, stale.stale], [[], true]);
  assert.equal(stale.label, 'peer c3d4, not seen for 7 minutes, last seen with 5 runners', 'says what the row shows');
  // An older daemon sends only the count: every runner idle, claiming no work.
  assert.deepEqual(peer(40, { runners: { 'owner/a': 2 } }).pips.map((p) => p.kind), ['idle', 'idle']);
  assert.deepEqual(peer(40, { runners: {} }).pips, []);
});

test('vmSection: stale lists dim every dot and age, and say how old; missing fields degrade', async () => {
  const { vmSection } = await server.ssrLoadModule('/src/popover/vms.ts');
  const s0 = Math.floor(fx.NOW / 1000);
  const vms = [
    { kind: 'linux', slot: 0, state: 'in_service', started_unix: s0 - 600, cpus: 6, memory_mib: 16384 },
    { kind: 'macos', slot: 0, state: 'booting', started_unix: s0 - 10, cpus: 4, memory_mib: 8192 },
  ];
  const stale = vmSection({ docker_inflight: 2, vm: { listed_age_s: 180, vms } }, fx.NOW);
  assert.equal(stale.dim, true);
  assert.equal(stale.asOf, 'as of 3m ago');
  assert.ok(stale.rows.every((r) => r.tone === 'neutral' && r.age === '—' && r.doing === ''), 'stale: name only');
  assert.ok(stale.rows.every((r) => r.status === 'unknown'), 'stale: no state drawn, a dashed ring');
  assert.ok(stale.rows.every((r) => !r.facts.some((f) => f.label === 'State')), 'stale: no State line');
  assert.deepEqual(stale.rows[0].facts.map((f) => f.label), ['CPU', 'Memory'], 'sizes stay: they do not change while a VM runs');
  assert.equal(vmSection({ vm: { listed_age_s: 30, vms } }, fx.NOW).dim, false, 'stale only past 30 s');

  // An older daemon: no sizes, no start time, no age.
  const old = vmSection({ vm: { vms: [{ kind: 'linux', slot: 0, state: 'in_service' }] } }, fx.NOW);
  assert.equal(old.dim, false);
  assert.deepEqual([old.rows[0].facts, old.rows[0].age], [[], '—'], 'nothing to open');
  assert.deepEqual(vmSection({}, fx.NOW).rows, []);
  assert.deepEqual(vmSection(undefined, fx.NOW).rows, []);
});

test('VMs do not make the popover non-idle: the idle fixture with a VM still has no group', () => {
  assert.equal(build(fx.idleVm).length, 0);
});

test('dates read the same in every locale: 24-hour, English month, year only when not this year', () => {
  const now = new Date(2026, 9, 28, 9, 0).getTime();
  assert.equal(format.fmtDateTime(new Date(2026, 9, 28, 13, 0).getTime(), now), '28 Oct, 13:00');
  assert.equal(format.fmtDateTime(new Date(2026, 0, 5, 7, 5).getTime(), now), '5 Jan, 07:05');
  assert.equal(format.fmtDateTime(new Date(2025, 11, 31, 23, 59).getTime(), now), '31 Dec 2025, 23:59');
  assert.equal(format.fmtDateTime(undefined, now), '');
  assert.equal(format.fmtClock(new Date(2026, 9, 28, 0, 7).getTime()), '00:07');
});

// --- ADR-0064: dismissals and trust acknowledgement ----------------------------------------------

test('blocking rows carry no Dismiss and are never hidden by a dismissal', () => {
  const s = fx.attention.state;
  const rows = attention.daemonRows(s, fx.NOW);
  for (const key of ['unhealthy', 'hold', 'repo-hold:owner/demo', 'draining', 'missing']) {
    const r = rows.find((x) => x.key === key);
    assert.ok(r?.blocking, `${key} is blocking`);
    assert.equal(attention.canDismiss(r), false);
  }
  const all = { dismissed: rows.map((r) => ({ key: r.key, ident: attention.rowIdent(r), at_ms: 0 })) };
  const { active, dismissed } = attention.splitDismissed(rows, all);
  assert.deepEqual(dismissed, []);
  assert.equal(active.length, rows.length);
});

test('macOS refusals and golden images block only while jobs wait; a retired runner blocks, a retiring one does not', () => {
  const held = attention.daemonRows(fx.macosHeld.state, fx.NOW);
  assert.equal(held.find((r) => r.key === 'macos-refusal:vm_session').blocking, true);
  assert.equal(held.find((r) => r.key === 'golden').blocking, false);
  const retire = (d) => attention.daemonRows({ runner_retirements: [{ profile: 'linux', days_remaining: d, urgent: true }] }, fx.NOW)[0];
  assert.equal(retire(-1).blocking, true);
  assert.equal(retire(2).blocking, false);
});

test('a dismissal follows the row identity: moving figures keep it hidden, new words show it', () => {
  const row = (cores) => ({ key: 'warning:cpu:x', sev: 'warn', icon: 'alert', parts: [`x wants ${cores} cores, held at 4`] });
  assert.equal(attention.rowIdent(row('6')), 'x wants # cores, held at #');
  assert.equal(attention.rowIdent(row('6.5')), 'x wants #.# cores, held at #');
  const att = { dismissed: [{ key: 'warning:cpu:x', ident: attention.rowIdent(row('6')), at_ms: 0 }] };
  assert.equal(attention.splitDismissed([row('8')], att).dismissed.length, 1);
  const other = { ...row('6'), parts: ['x OOM-killed, held at 4'] };
  assert.equal(attention.splitDismissed([other], att).active.length, 1);
});

test('update rows are identified by their version', () => {
  const [staged] = attention.updateRows({ available: '0.4.2', phase: { phase: 'waiting_idle', version: '0.4.2' } });
  assert.equal(attention.rowIdent(staged), '0.4.2');
  const [wait] = attention.updateRows({ available: '0.4.3', install_in_secs: 600 });
  assert.equal(attention.rowIdent(wait), '0.4.3');
  assert.equal(attention.appUpdateRows({ updated_to: '0.4.1' })[0].ident, '0.4.1');
});

test('acknowledged trust changes fold into one quiet row; a newer one keeps Acknowledge up to the newest', () => {
  const s = fx.trustAcked.state;
  const { active, dismissed } = attention.applyAttention(attention.trustRows(s, false, fx.NOW), s.attention);
  assert.deepEqual(dismissed, []);
  const fold = active.find((r) => r.key === attention.TRUST_ACKED_KEY);
  assert.equal(fold.sev, 'info');
  assert.equal(fold.details.lines.length, 2);
  assert.match(attention.plainText(fold.parts), /^2 trust changes this week, acknowledged/);
  assert.equal(attention.canDismiss(fold), false);
  const open = active.filter((r) => r.key.startsWith('trust-') && r.key !== attention.TRUST_ACKED_KEY);
  assert.equal(open.length, 1);
  assert.equal(attention.canDismiss(open[0]), false);
  const ack = open[0].actions.find((a) => a.cmd === 'ack_trust');
  assert.deepEqual(ack.args, { untilMs: Math.max(...s.trust_changes.map((c) => c.ts_ms)) });
  // Nothing acknowledged: every change is a full row, none folded.
  const none = attention.applyAttention(attention.trustRows({ ...s, attention: undefined }, false, fx.NOW), undefined);
  assert.equal(none.active.length, 3);
  assert.ok(!none.active.some((r) => r.key === attention.TRUST_ACKED_KEY));
});

test('the dismissed fixture: Paused stays, two rows fold away, and the tally counts only what shows', () => {
  const s = fx.dismissed.state;
  const { active, dismissed } = attention.applyAttention(attention.daemonRows(s, fx.NOW), s.attention);
  assert.deepEqual(dismissed.map((r) => r.key).sort(), ['bypass', 'oom-held']);
  assert.deepEqual(active.map((r) => r.key).sort(), ['paused', 'refused']);
  assert.deepEqual(attention.severityTally(active), [{ sev: 'warn', count: 2 }]);
});

test('a retirement group is dismissed as a whole: a profile joining it shows the row again', () => {
  const ret = (...profiles) => ({
    runner_retirements: profiles.map((profile) => ({ profile, days_remaining: 2, urgent: true, cutoff: '2026-12-01' })),
  });
  const rows = (s) => attention.daemonRows(s, fx.NOW).filter((r) => r.key.startsWith('retire:'));
  const one = rows(ret('linux'));
  const att = { dismissed: [{ key: one[0].key, ident: attention.rowIdent(one[0]), at_ms: 0 }] };
  assert.equal(attention.splitDismissed(one, att).dismissed.length, 1);
  const two = rows(ret('linux', 'macos'));
  const split = attention.splitDismissed(two, att);
  assert.equal(split.dismissed.length, 0);
  assert.equal(split.active.length, 1);
});
