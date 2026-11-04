// The dashboard's data layer (ui/src/dashboard/): the alarm model against the inline page's notice
// triggers, the popover's own rows left as they were, the daemon client's session handling, and
// the polling hook's staleness rule. Loaded through Vite's SSR loader, as popover.test.mjs is.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, afterEach, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let alarms;
let screen;
let fx;
let attention;
let api;
let session;
let poll;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  alarms = await server.ssrLoadModule('/src/dashboard/alarms.ts');
  screen = await server.ssrLoadModule('/src/dashboard/screen.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
  attention = await server.ssrLoadModule('/src/popover/attention.ts');
  api = await server.ssrLoadModule('/src/dashboard/api.ts');
  session = await server.ssrLoadModule('/src/session.ts');
  poll = await server.ssrLoadModule('/src/dashboard/useDaemonState.ts');
});
after(() => server?.close());

const clone = (x) => structuredClone(x);
const NOW = () => fx.NOW;
/** The quiet host with one thing changed. */
const with_ = (patch) => ({ ...clone(fx.quiet), ...patch });
const budget = (patch) => with_({ budget: { ...clone(fx.quiet.budget), ...patch } });
const update = (patch) => with_({ update: { ...clone(fx.quiet.update), ...patch } });
const cred = (patch) => with_({ credential: { ...clone(fx.quiet.credential), ...patch } });
const run = (s) => alarms.dashboardAlarms(s, NOW());
const keys = (s) => run(s).map((a) => a.key);
const only = (s) => run(s).map((a) => [a.key, a.sev]);
const text = (a) => a.parts.map((p) => (typeof p === 'string' ? p : (p.strong ?? p.mono ?? ' | '))).join('');

// --- (a) each trigger of the inline page, alone -------------------------------------------------

const mm = (granted, opted) => ({ granted_not_opted_in: granted, opted_in_not_granted: opted });
const TRIGGERS = [
  ['credential missing', () => cred({ status: 'missing', reason: 'no token stored' }), 'credential-missing', 'bad'],
  ['credential invalid', () => cred({ status: 'invalid', reason: 'GitHub refused it' }), 'credential-invalid', 'bad'],
  ['credential expiring', () => cred({ status: 'expiring', reason: 'expires in 5 days' }), 'credential-expiring', 'warn'],
  ['credential file warning', () => cred({ file_warning: 'group-readable' }), 'credential-file', 'warn'],
  [
    'credential missing permission',
    () => cred({ missing_permissions: [{ repo: 'acme/mobile', permission: 'checks: read', feature: 'auto-retry', fix: 'add it' }] }),
    'credential-perm:acme/mobile:checks: read',
    'warn',
  ],
  ['token reaches repos not opted in', () => cred({ mismatches: mm(['acme/api'], []) }), 'credential-granted-not-opted-in', 'warn'],
  ['opted in but not granted', () => cred({ mismatches: mm([], ['owner/demo']) }), 'credential-opted-in-not-granted', 'warn'],
  ['credential unverified permissions', () => cred({ unverified_permissions: ['administration: write'] }), 'credential-unverified', 'info'],
  ['gate allows public repos', () => with_({ gate: { ...fx.quiet.gate, allow_public_repos: true } }), 'gate-public', 'warn'],
  ['daemon notice', () => with_({ notices: [{ kind: 'legacy_layout', text: 'Still on the LEGACY layout.' }] }), 'notice:legacy_layout', 'warn'],
  [
    'dind wall',
    () => with_({ profiles: [{ name: 'linux', labels: ['faber'], backend: 'docker', dind: 'unstated', needs_wall: true }] }),
    'dind-wall',
    'warn',
  ],
  ['dind label rule off', () => with_({ dind_label_rule: 'off' }), 'dind-rule', 'warn'],
  ['dind label rule unstated', () => with_({ dind_label_rule: 'unstated' }), 'dind-rule', 'warn'],
  ['unqualified image', () => with_({ unqualified_images: [{ profile: 'linux', image: 'runner:latest', message: 'pulls from a user namespace' }] }), 'unqualified:linux', 'warn'],
  [
    'git mirror blocked',
    () => with_({ git_mirrors: [{ repo: 'acme/api', blocked: 'is held by an unlabelled volume' }] }),
    'git-mirror:acme/api',
    'bad',
  ],
  ['git mirror refresh failed', () => with_({ git_mirrors: [{ repo: 'acme/web', ready: true, last_error: 'fetch failed' }] }), 'git-mirror:acme/web', 'warn'],
  [
    'runner retirement, urgent',
    () => with_({ runner_retirements: [{ profile: 'linux', days_remaining: 2, urgent: true, message: 'stops soon' }] }),
    'retire:linux',
    'bad',
  ],
  [
    'runner retirement, later',
    () => with_({ runner_retirements: [{ profile: 'linux', days_remaining: 20, urgent: false, message: 'stops later' }] }),
    'retire:linux',
    'warn',
  ],
  ['budget warning', () => budget({ warnings: ['a cap awaits a pin'] }), 'warning:0', 'warn'],
  ['peer instances', () => with_({ peers: [{ instance: 'beta', runners: { linux: 2 } }] }), 'peers', 'info'],
  ['idle sleep error', () => with_({ awake: { enabled: true, supported: true, held: false, error: 'assertion failed' } }), 'awake-error', 'warn'],
  ['idle sleep unsupported', () => with_({ awake: { enabled: true, supported: false } }), 'awake-unsupported', 'info'],
  ['update downloading', () => update({ phase: { phase: 'downloading', version: '0.4.2' } }), 'upd-downloading', 'info'],
  ['update deferred', () => update({ phase: { phase: 'deferred', version: '0.4.2', reason: 'config incompatible' } }), 'upd-deferred', 'warn'],
  ['update failed', () => update({ phase: { phase: 'failed', reason: 'bad signature' }, next_attempt_ms: fx.NOW + 60000 }), 'upd-failed', 'bad'],
  ['update draining', () => update({ phase: { phase: 'draining', version: '0.4.2' } }), 'upd-draining', 'warn'],
  ['update available', () => update({ available: '0.4.2' }), 'upd-available', 'info'],
  ['update pinned', () => update({ pin: '0.4.1' }), 'upd-pin', 'info'],
];

for (const [name, make, key, sev] of TRIGGERS) {
  test(`trigger alone: ${name}`, () => {
    assert.deepEqual(only(make()), [[key, sev]]);
  });
}

// The same shape, through the popover's producers: still alarms, with the popover's keys and tones.
const PASS_THROUGH = [
  ['wall red', () => budget({ vm: { wall: 'red', wall_detail: 'anchor missing' } }), 'wall-red', 'bad'],
  ['update staged', () => update({ available: '0.4.2', phase: { phase: 'waiting_idle', version: '0.4.2' } }), 'upd-staged', 'warn'],
  ['update maturity wait', () => update({ available: '0.4.2', install_in_secs: 7200 }), 'upd-wait', 'warn'],
  ['rolled back', () => update({ rolled_back_from: '0.4.0' }), 'upd-rb', 'bad'],
  ['docker hold', () => budget({ hold: { reason: 'maintenance', since_ms: fx.NOW - 60000 } }), 'hold', 'warn'],
  ['repo hold', () => with_({ repo_holds: [{ repo: 'owner/demo', reason: 'bypass', since_ms: fx.NOW - 60000, by: 'gate' }] }), 'repo-hold:owner/demo', 'warn'],
  ['trust change', () => with_({ trust_changes: [{ ts_ms: fx.NOW - 60000, key: 'k', sentence: 'public repos were allowed', via: 'session' }] }), () => `trust-${fx.NOW - 60000}-k`, 'warn'],
];
for (const [name, make, key, sev] of PASS_THROUGH) {
  test(`trigger alone, popover producer: ${name}`, () => {
    assert.deepEqual(only(make()), [[typeof key === 'function' ? key() : key, sev]]);
  });
}

test('every missing image is its own alarm, where the popover folds them into "+N more"', () => {
  const s = with_({
    missing_images: [
      { profile: 'linux', image: 'registry.example/acme/runner:1', since_ms: fx.NOW - 1000, cause: 'denied' },
      { profile: 'macos', image: 'registry.example/acme/runner-mac:1' },
    ],
  });
  assert.deepEqual(keys(s), ['missing:linux:registry.example/acme/runner:1', 'missing:macos:registry.example/acme/runner-mac:1']);
  assert.ok(run(s).every((a) => a.sev === 'bad'));
  assert.equal(attention.daemonRows(s, fx.NOW).filter((r) => r.key === 'missing').length, 1);
});

test('every encroach line is shown, not the first alone', () => {
  const s = budget({ machine: { ...fx.quiet.budget.machine, encroaches: ['first fix', 'second fix'] } });
  assert.deepEqual(only(s), [
    ['vm-large', 'warn'],
    ['vm-large:1', 'warn'],
  ]);
});

test('every retiring profile is listed, with the daemon\'s own message', () => {
  const s = with_({
    runner_retirements: [
      { profile: 'macos', days_remaining: 20, urgent: false, message: 'macos message' },
      { profile: 'linux', days_remaining: 2, urgent: true, message: 'linux message' },
    ],
  });
  const list = run(s);
  assert.deepEqual(list.map((a) => a.key), ['retire:linux', 'retire:macos']);
  assert.match(text(list[0]), /linux message/);
});

test('mirror rows name the jobs an outage affected', () => {
  const s = with_({
    mirror: { attention: [{ key: 'mirror-down', severity: 'bad', title: 'Mirror down', detail: 'not answering', affected: ['acme/web CI unit', 'acme/api CI e2e'] }] },
  });
  const [a] = run(s);
  assert.equal(a.key, 'mirror-down');
  assert.equal(a.sev, 'bad');
  assert.ok(a.parts.some((p) => p.mono === 'acme/web CI unit') && a.parts.some((p) => p.mono === 'acme/api CI e2e'));
});

test('a required wall that is red also stops Docker jobs, and says so', () => {
  const s = budget({ vm: { wall: 'red', wall_detail: 'anchor missing', wall_required: true } });
  const [a] = run(s);
  assert.equal(a.key, 'wall-red');
  assert.match(text(a), /no Docker job/);
});

test('update actions: Install now / Skip, Skip, Check now', () => {
  const cmds = (s, key) => run(s).find((a) => a.key === key).actions.map((x) => x.cmd);
  assert.deepEqual(cmds(update({ available: '0.4.2', phase: { phase: 'waiting_idle', version: '0.4.2' } }), 'upd-staged'), ['apply_update', 'skip_update']);
  assert.deepEqual(cmds(update({ available: '0.4.2', install_in_secs: 7200 }), 'upd-wait'), ['skip_update']);
  assert.deepEqual(cmds(update({ available: '0.4.2' }), 'upd-available'), ['check_update']);
});

test('hold and repo-hold alarms carry Release and Clear', () => {
  const held = with_({
    budget: { ...clone(fx.quiet.budget), hold: { reason: 'maintenance', since_ms: fx.NOW - 60000 } },
    repo_holds: [{ repo: 'owner/demo', reason: 'bypass', since_ms: fx.NOW - 60000, by: 'gate' }],
  });
  const list = run(held);
  assert.deepEqual(list.find((a) => a.key === 'hold').actions.map((x) => x.cmd), ['unhold']);
  const clear = list.find((a) => a.key === 'repo-hold:owner/demo').actions[0];
  assert.equal(clear.cmd, 'clear_repo_hold');
  assert.deepEqual(clear.args, { repo: 'owner/demo' });
  assert.ok(list.flatMap((a) => (a.actions || []).map((x) => x.cmd)).every((c) => alarms.ALARM_CMDS.includes(c)));
});

test('an OOM-held class keeps its Pin: the cmd is run by the dashboard, and the pin asks first', () => {
  const held = { key: 'acme/api|ci|e2e', label: 'acme/api · ci · e2e', cap_mib: 8192, pin_runner_mib: 9216, pin_dind_mib: 3072, ooms: 2, runs: 3 };
  const list = alarms.dashboardAlarms(budget({ oom_held: [held] }), NOW());
  const row = list.find((a) => a.key === 'oom-held');
  const pin = row.actions[0];
  assert.equal(pin.cmd, 'set_pin');
  assert.ok(alarms.ALARM_CMDS.includes(pin.cmd));
  assert.ok(pin.confirm?.question && pin.confirm.consequence && pin.confirm.label, 'a risk-accepting action carries its confirmation (#385)');
  assert.deepEqual(screen.alarmAction(pin.cmd, pin.args), {
    cmd: 'pin',
    req: { class: 'acme/api|ci|e2e', pin: true, runner_mib: 9216, dind_mib: 3072 },
  });
  // Malformed args are not run.
  assert.equal(screen.alarmAction('set_pin', { class: 'x' }), null);
});

test('the other trigger clauses stay quiet', () => {
  assert.deepEqual(keys(with_({ dind_label_rule: 'on' })), []);
  assert.deepEqual(keys(with_({ dind_label_rule: undefined })), []);
  assert.deepEqual(keys(with_({ awake: { enabled: false, supported: false, error: 'x' } })), []);
  assert.deepEqual(keys(with_({ awake: { enabled: true, supported: true, held: true, error: 'x' } })), []);
  // A healthy mirror, and a stored token, are data, not alarms.
  assert.deepEqual(keys(with_({ git_mirrors: [{ repo: 'acme/web', ready: true, last_refresh_ms: fx.NOW }] })), []);
  // Unverified permissions are information only while nothing else is wrong with the token.
  assert.deepEqual(keys(cred({ file_warning: 'x', unverified_permissions: ['administration: write'] })), ['credential-file']);
  // The maturity row steps aside for a download, a deferral or a failure, as on the inline page.
  assert.deepEqual(keys(update({ available: '0.4.2', install_in_secs: 60, phase: { phase: 'downloading', version: '0.4.2' } })), ['upd-downloading']);
  // Two notices of one kind keep distinct keys.
  const two = with_({ notices: [{ kind: 'k', text: 'a' }, { kind: 'k', text: 'b' }] });
  assert.deepEqual(keys(two), ['notice:k', 'notice:k:1']);
});

test('daemon text stays plain text: parts are strings or strong/mono/sep, never markup', () => {
  const hostile = '<img src=x onerror=alert(1)>';
  const s = with_({ notices: [{ kind: 'k', text: hostile }], credential: { ...fx.quiet.credential, status: 'invalid', reason: hostile } });
  for (const a of run(s)) {
    for (const p of a.parts) assert.ok(typeof p === 'string' || 'strong' in p || 'mono' in p || 'sep' in p);
  }
  assert.ok(run(s).some((a) => a.parts.includes(hostile)));
});

// --- (b) the healthy host ----------------------------------------------------------------------

test('a quiet, healthy host raises nothing', () => {
  assert.deepEqual(run(fx.quiet), []);
});

// --- the full fixture trips every alarm --------------------------------------------------------

test('the full fixture raises every dashboard alarm once, worst first', () => {
  const list = run(fx.full);
  const k = list.map((a) => a.key);
  assert.equal(new Set(k).size, k.length, 'keys are unique');
  const expected = [
    'credential-expiring',
    'credential-file',
    'credential-perm:acme/mobile:checks: read',
    'credential-granted-not-opted-in',
    'credential-opted-in-not-granted',
    'gate-public',
    'notice:legacy_layout',
    'notice:linux_vm_waiting',
    'dind-wall',
    'dind-rule',
    'missing:linux:registry.example/acme/runner:2.330.0',
    'missing:macos:registry.example/acme/runner-mac:1',
    'unqualified:linux',
    'git-mirror:acme/web',
    'git-mirror:acme/api',
    'mirror-down',
    'mirror-image',
    'retire:linux',
    'retire:macos',
    'vm-large',
    'vm-large:1',
    'warning:0',
    'peers',
    'awake-error',
    'upd-failed',
    'upd-pin',
    'upd-rb',
    'upd-sc',
    'upd-sup',
    'wall-red',
    'repo-hold:owner/demo',
    'hold',
    'unhealthy',
    'paused',
    'draining',
    'bypass',
    'refused',
  ];
  for (const e of expected) assert.ok(k.includes(e), `missing ${e}`);
  const rank = { bad: 0, warn: 1, info: 2 };
  assert.deepEqual(list.map((a) => rank[a.sev]), list.map((a) => rank[a.sev]).sort((a, b) => a - b));
});

// ADR-0064 § 3: one dismissal holds on every surface, so a row the popover and the dashboard both
// show, under one key, has one identity, however each words it.
test('a row on both surfaces is identified alike, so a dismissal holds on both', () => {
  const s = {
    ...fx.full,
    mirror: { ...fx.full.mirror, attention: (fx.full.mirror?.attention || []).map((r) => ({ ...r, affected: ['job-a'] })) },
    runner_retirements: [
      { profile: 'linux', days_remaining: 2, urgent: true, cutoff: '2026-12-01', runner_version: '2.1' },
      { profile: 'macos', days_remaining: 20, urgent: false, cutoff: '2026-12-20', runner_version: '2.1' },
    ],
  };
  const pop = [...attention.daemonRows(s, NOW()), ...attention.updateRows(s.update)];
  const dash = new Map(run(s).map((a) => [a.key, a]));
  // Only a dismissable row's identity is read; a blocking one (the wall) may be worded apart.
  const shared = pop.filter((r) => dash.has(r.key) && attention.canDismiss(r));
  for (const k of ['vm-large', 'retire:linux', 'retire:macos', ...s.mirror.attention.map((r) => r.key)]) {
    assert.ok(shared.some((r) => r.key === k), `${k} is on both surfaces`);
  }
  for (const r of shared) assert.equal(attention.rowIdent(dash.get(r.key)), attention.rowIdent(r), `identity of ${r.key}`);
});

// --- (c) the popover's rows are the popover's ---------------------------------------------------

test('the popover producers are unchanged and their rows reach the dashboard intact', () => {
  for (const s of [fx.full, fx.quiet]) {
    const before = JSON.stringify(s);
    const direct = {
      daemon: attention.daemonRows(s, fx.NOW),
      update: attention.updateRows(s.update),
      trust: attention.trustRows(s, undefined, fx.NOW),
    };
    const snapshot = JSON.stringify(direct);
    const list = run(s);
    assert.equal(JSON.stringify(s), before, 'the state is not mutated');
    assert.equal(
      JSON.stringify({
        daemon: attention.daemonRows(s, fx.NOW),
        update: attention.updateRows(s.update),
        trust: attention.trustRows(s, undefined, fx.NOW),
      }),
      snapshot,
      'the producers answer the same after the dashboard has composed them',
    );
    // Rows the dashboard words itself step aside; the rest appear verbatim, popover wording included.
    const aside = new Set(['missing', 'vm-large', 'wall-red', 'upd-wait', ...(s.mirror?.attention || []).map((r) => r.key)]);
    for (const r of [...direct.daemon, ...direct.update, ...direct.trust].filter((r) => !aside.has(r.key) && !r.key.startsWith('retire:'))) {
      assert.deepEqual(list.find((a) => a.key === r.key), r, `popover row ${r.key}`);
    }
  }
});

// --- (d) the client -----------------------------------------------------------------------------

const realFetch = globalThis.fetch;
let calls;
const answer = (status, body, json = false) => {
  calls = [];
  globalThis.fetch = async (url, init) => {
    calls.push({ url, init });
    return new Response(json ? JSON.stringify(body) : body, { status });
  };
};
const store = (values) => {
  Object.defineProperty(globalThis, 'sessionStorage', { configurable: true, value: { getItem: (k) => values[k] ?? null } });
};
afterEach(() => {
  globalThis.fetch = realFetch;
  delete globalThis.sessionStorage;
});

test('getState sends the session key and the cookie', async () => {
  store({ 'faber-key': 'test-key' });
  answer(200, { paused: false }, true);
  assert.deepEqual(await api.getState(), { paused: false });
  assert.equal(calls[0].url, '/api/state');
  assert.equal(calls[0].init.headers['X-Faber-Session'], 'test-key');
  assert.equal(calls[0].init.credentials, 'same-origin');
});

test('blocked storage still sends the header, empty, and the daemon decides', async () => {
  Object.defineProperty(globalThis, 'sessionStorage', {
    configurable: true,
    get() {
      throw new Error('blocked');
    },
  });
  answer(200, {}, true);
  await api.getState();
  assert.equal(calls[0].init.headers['X-Faber-Session'], '');
});

test('a 401 is SessionEnded, with the reason the inline page gave', async () => {
  store({ 'faber-key': 'k', 'faber-since': String(Date.now() - 3600 * 1000) });
  answer(401, 'no');
  await assert.rejects(api.getState(), (e) => {
    assert.ok(api.isSessionEnded(e));
    assert.equal(e.why, 'The daemon restarted (an update or a restart)');
    return true;
  });
  store({ 'faber-since': String(Date.now() - 13 * 3600 * 1000) });
  await assert.rejects(api.reconcile(), (e) => api.isSessionEnded(e) && e.why === 'Session older than 12 h');
  assert.equal(session.sessionEndedWhy(null, Date.now()), 'Session older than 12 h');
});

test('every action posts to the route and body the handler reads', async () => {
  store({ 'faber-key': 'k' });
  const table = [
    [() => api.reconcile(), '/api/reconcile', undefined],
    [() => api.pause(), '/api/pause', undefined],
    [() => api.resume(), '/api/resume', undefined],
    [() => api.drain(), '/api/drain', undefined],
    [() => api.hold('why'), '/api/hold', { reason: 'why' }],
    [() => api.unhold(), '/api/unhold', undefined],
    [() => api.clearRepoHold('owner/demo'), '/api/repo-hold/clear', { repo: 'owner/demo' }],
    [() => api.setPin({ class: 'c', pin: true, runner_mib: 1, dind_mib: 2, runner_cpus: 1, dind_cpus: 0 }), '/api/pin', { class: 'c', pin: true, runner_mib: 1, dind_mib: 2, runner_cpus: 1, dind_cpus: 0 }],
    [() => api.updateApply(), '/api/update/apply', undefined],
    [() => api.updateSkip(), '/api/update/skip', undefined],
    [() => api.updateCheck(), '/api/update/check', undefined],
    [() => api.goldenVerify('macos-golden'), '/api/golden/verify', { image: 'macos-golden' }],
  ];
  for (const [fn, url, body] of table) {
    answer(200, '{}');
    await fn();
    const { init, url: sent } = calls[0];
    assert.equal(sent, url);
    assert.equal(init.method, 'POST');
    assert.equal(init.headers['X-Faber-Session'], 'k');
    if (body === undefined) assert.equal(init.body, undefined, url);
    else {
      assert.deepEqual(JSON.parse(init.body), body);
      assert.equal(init.headers['Content-Type'], 'application/json');
    }
  }
});

test('state-returning actions return the state; word actions return the word', async () => {
  store({});
  answer(200, { paused: true }, true);
  assert.deepEqual(await api.hold('x'), { paused: true });
  answer(200, 'draining 2\n');
  assert.equal(await api.drain(), 'draining 2');
});

test('a refusal throws an Error carrying the daemon\'s text', async () => {
  store({});
  answer(403, 'clearing a hold needs sudo\n');
  await assert.rejects(api.clearRepoHold('owner/demo'), (e) => e instanceof Error && !api.isSessionEnded(e) && e.message === 'clearing a hold needs sudo' && e.status === 403);
  answer(400, 'a hold needs a reason');
  await assert.rejects(api.hold(''), /a hold needs a reason/);
  answer(404, 'no `macos-vm` profile names the image x');
  await assert.rejects(api.goldenVerify('x'), /no `macos-vm` profile/);
  answer(500, '');
  await assert.rejects(api.getState(), /faber answered 500/);
});

// --- the polling hook's staleness rule ---------------------------------------------------------

test('stale: never answered, failed, or older than 6 s', () => {
  assert.equal(poll.isStale(null, false, 1000), true);
  assert.equal(poll.isStale(1000, true, 1500), true);
  assert.equal(poll.isStale(1000, false, 1000 + poll.STALE_MS), false);
  assert.equal(poll.isStale(1000, false, 1001 + poll.STALE_MS), true);
  assert.equal(poll.POLL_MS, 2000);
});

// Moved from the retired inline page's Rust test (the_page_quotes_the_repo_of_a_sudo_command): a
// copyable command built from a name the daemon or GitHub supplied quotes that name, so pasting it
// runs exactly one command. The class, not one input: every shell metacharacter, and a quote.
test('a copyable sudo command quotes the repo it names, whatever the name holds', async () => {
  const fmt = await server.ssrLoadModule('/src/popover/format.ts');
  for (const hostile of ['acme/web; rm -rf ~', 'acme/$(id)', 'acme/`id`', "acme/it's", 'acme/a b', 'acme/x|y', 'acme/x&y', 'acme/x>y', 'acme/x\ny']) {
    const word = fmt.shellWord(hostile);
    assert.match(word, /^'.*'$/s, `${JSON.stringify(hostile)} went out unquoted: ${word}`);
    // Inside single quotes only a quote can end the word, and it is closed, escaped and reopened.
    assert.equal(word.slice(1, -1).replace(/'\\''/g, ''), hostile.replace(/'/g, ''), word);
  }
  assert.equal(fmt.shellWord('acme/web'), 'acme/web');
  const s = { repo_holds: [{ repo: 'acme/web; rm -rf ~', reason: 'a bypass', since_ms: fx.NOW }] };
  const row = alarms.dashboardAlarms(s, fx.NOW).find((a) => a.key.startsWith('repo-hold:'));
  const cmd = row.parts.find((p) => typeof p === 'object' && p.mono?.startsWith('sudo '));
  assert.equal(cmd.mono, "sudo faber repo-hold clear 'acme/web; rm -rf ~'");
});

test('update alarms: a fallback note never trails a dash, and a given reason does', () => {
  const dash = (u, key) => text(run(update(u)).find((a) => a.key === key));
  const bare = dash({ phase: { phase: 'deferred', version: '0.4.2' } }, 'upd-deferred');
  assert.match(bare, /0\.4\.2 held back/);
  assert.doesNotMatch(bare, /—|Deferred/);
  const failed = dash({ phase: { phase: 'failed' } }, 'upd-failed');
  assert.doesNotMatch(failed, /—|No reason given/);
  assert.match(dash({ phase: { phase: 'failed', reason: 'bad signature' } }, 'upd-failed'), /Update failed — bad signature/);
});

test('update alarms and the Updates panel agree on every phase (same words, the alarm adds the version)', async () => {
  const m = await server.ssrLoadModule('/src/dashboard/maintenance.ts');
  for (const phase of ['downloading', 'draining', 'deferred', 'failed']) {
    const s = update({ available: '0.4.2', phase: { phase, version: '0.4.2', reason: 'a reason' } });
    const panel = m.updateModel(s, fx.NOW).phase;
    const a = run(s).find((x) => x.key === `upd-${phase}`);
    const norm = (t) => t.replaceAll('0.4.2', '').replace('…', '').replace(/\s+/g, ' ').trim().toLowerCase();
    assert.equal(norm(text(a).split(' — ')[0]), norm(panel.word), phase);
    assert.match(text(a).split(' — ')[0], phase === 'failed' ? /^Update failed$/ : /0\.4\.2/, `${phase}: the alarm stands alone, so it names the version`);
  }
});

// --- ADR-0064: dismissals ------------------------------------------------------------------------

test('budget warnings are keyed by the daemon, falling back to their index on an older daemon', () => {
  const items = [{ key: 'memory:acme/api|CI|e2e', text: 'a cap awaits a pin' }];
  assert.deepEqual(only(budget({ warnings: ['a cap awaits a pin'], warning_items: items })), [['warning:memory:acme/api|CI|e2e', 'warn']]);
  assert.deepEqual(keys(budget({ warnings: ['a', 'b'] })), ['warning:0', 'warning:1']);
});

test('alarms that stop jobs are blocking; the rest are dismissable', () => {
  const list = run(fx.full);
  const blocking = new Set(list.filter((a) => a.blocking).map((a) => a.key));
  for (const k of ['credential-opted-in-not-granted', 'missing:linux:registry.example/acme/runner:2.330.0', 'git-mirror:acme/api', 'paused', 'draining', 'hold', 'unhealthy', 'frozen', 'machine', 'frozen-disk']) {
    assert.ok(blocking.has(k), `${k} is blocking`);
  }
  for (const k of ['gate-public', 'dind-wall', 'credential-expiring', 'retire:linux', 'peers', 'awake-error']) {
    assert.ok(!blocking.has(k), `${k} is dismissable`);
  }
  assert.ok(run(cred({ status: 'missing', reason: 'x' }))[0].blocking);
  assert.ok(run(with_({ runner_retirements: [{ profile: 'linux', days_remaining: -1, urgent: true }] }))[0].blocking);
});

test('dismissed alarms leave the active list; the trust change folds acknowledged', () => {
  const { active, dismissed } = alarms.activeAlarms(fx.dismissedAlarms, NOW());
  assert.deepEqual(dismissed.map((a) => a.key).sort(), ['dind-wall', 'gate-public', 'warning:memory:acme/api|CI|e2e']);
  assert.ok(active.some((a) => a.key === 'warning:cpu:acme/mobile|CI|build'));
  assert.ok(active.some((a) => a.key === attention.TRUST_ACKED_KEY));
  assert.ok(!active.some((a) => /^trust-\d/.test(a.key)));
});

test('Dismiss, Restore and Acknowledge map onto the daemon routes', () => {
  for (const c of ['dismiss_attention', 'restore_attention', 'ack_trust']) assert.ok(alarms.ALARM_CMDS.includes(c));
  assert.deepEqual(screen.alarmAction('dismiss_attention', { key: 'k', ident: 'i' }), { cmd: 'dismiss_attention', key: 'k', ident: 'i' });
  assert.equal(screen.alarmAction('dismiss_attention', { key: 'k' }), null);
  assert.deepEqual(screen.alarmAction('restore_attention', { key: 'k' }), { cmd: 'restore_attention', key: 'k' });
  assert.deepEqual(screen.alarmAction('restore_attention', {}), { cmd: 'restore_attention' });
  assert.deepEqual(screen.alarmAction('ack_trust', { untilMs: 5 }), { cmd: 'ack_trust', until_ms: 5 });
  assert.deepEqual(screen.alarmAction('ack_trust', { untilMs: null }), { cmd: 'ack_trust', until_ms: null });
  assert.equal(screen.alarmAction('ack_trust', {}), null);
});
