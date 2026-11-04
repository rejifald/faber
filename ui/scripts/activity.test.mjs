// The Activity view's data shaping (ui/src/dashboard/activity.ts): the merge of every kind, the
// kind and result mapping, facet counts, filter semantics, search and the query-string round trip.
// Loaded through Vite's SSR loader, as dashboard.test.mjs is. Neutral names only: the repo is public.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let a;
let fx;

before(async () => {
  server = await createServer({ root: ui, configFile: false, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true, hmr: false } });
  a = await server.ssrLoadModule('/src/dashboard/activity.ts');
  fx = await server.ssrLoadModule('/src/dashboard/fixtures.ts');
});
after(() => server?.close());

const events = () => a.buildEvents(fx.full);
const byKind = (k) => events().filter((e) => e.kind === k);

test('every kind is merged, newest first, rows with no time first', () => {
  const evs = events();
  const kinds = new Set(evs.map((e) => e.kind));
  assert.deepEqual([...kinds].sort(), [...a.KINDS].sort());
  const times = evs.map((e) => e.ts_ms).filter((t) => t !== null);
  assert.deepEqual(times, [...times].sort((x, y) => y - x));
  const state = { ...fx.quiet, outcomes: [], recent: [], refused_queued: [{ repo: 'acme/web', job_id: 1, job_name: 'deploy', verdict: 'refuse:event:push' }] };
  const mixed = a.buildEvents({ ...state, faults: [{ ts_ms: 5, repo: 'acme/web', scope: 'docker', action: 'unreachable', cause: 'x' }] });
  assert.equal(mixed[0].kind, 'refused');
  assert.equal(mixed[0].ts_ms, null);
  assert.equal(a.whenLabel(mixed[0], fx.NOW), 'now');
  assert.equal(a.whenLabel(mixed[1], 65_000 + 5), '1m ago');
});

test('outcome results map to a label and tone', () => {
  const o = (result) => a.buildEvents({ ...fx.quiet, recent: [], outcomes: [{ ts_ms: 1, job_id: 1, result, repo: 'acme/web' }] })[0];
  assert.deepEqual([o('success').result, o('success').tone], ['Delivered', 'green']);
  assert.deepEqual([o('failure').result, o('failure').tone], ['Failed', 'red']);
  assert.deepEqual([o('infra_error').result, o('infra_error').tone], ['Infra error', 'red']);
  assert.deepEqual([o('timeout').result, o('timeout').tone], ['Timed out', 'amber']);
  assert.deepEqual([o('reclaimed').result, o('reclaimed').tone], ['Reclaimed', 'neutral']);
  assert.equal(o('weird_thing').result, 'Weird thing');
  const e = a.buildEvents({ ...fx.quiet, recent: [], outcomes: [{ ts_ms: 1, job_id: 7, result: 'infra_error', repo: 'acme/web', cause: 'first line\nsecond', duration_s: 3600, runner: 'r1' }] })[0];
  assert.equal(e.detail, 'first line');
  assert.equal(e.message, 'first line\nsecond');
  assert.equal(e.took, '1h');
  assert.deepEqual(e.fields.map((f) => f.key), ['runner', 'duration', 'job']);
});

test('decisions, faults, retries, drift, bypasses and refused map per source', () => {
  const dec = (verdict) => a.buildEvents({ ...fx.quiet, outcomes: [], recent: [{ ts_ms: 1, repo: 'acme/web', job_id: 1, event: 'push', actor: 'octo-dev', verdict, profile: 'linux' }] })[0];
  assert.equal(dec('provision').result, 'Provision');
  assert.equal(dec('skipped').result, 'Skipped');
  assert.equal(dec('refuse:actor:octo-bot').result, 'Refused');
  assert.equal(dec('refuse:actor:octo-bot').tone, 'amber');
  assert.equal(dec('provision').actor, 'octo-dev');
  const [f1, f2] = byKind('fault');
  assert.deepEqual([f1.result, f1.tone, f2.result, f2.tone].sort(), ['Failed', 'Recovered', 'green', 'red'].sort());
  assert.equal(byKind('retry')[0].result, 'Retried');
  assert.match(byKind('retry')[0].detail, /attempt 1/);
  assert.equal(byKind('drift')[0].tone, 'amber');
  assert.match(byKind('drift')[0].detail, /2\.331\.0/);
  assert.equal(byKind('bypass')[0].tone, 'red');
  assert.equal(byKind('refused').length, 3);
  assert.ok(byKind('refused').every((e) => e.result === 'Refused'));
});

test('facet counts come from the whole list; actor from decisions only', () => {
  const evs = events();
  const groups = a.buildFacets(evs);
  const g = Object.fromEntries(groups.map((x) => [x.id, x]));
  assert.equal(g.kind.items.reduce((n, i) => n + i.count, 0), evs.length);
  assert.equal(g.kind.items.find((i) => i.value === 'fault').count, 2);
  assert.equal(g.result.items.reduce((n, i) => n + i.count, 0), evs.length);
  assert.equal(g.repo.items.reduce((n, i) => n + i.count, 0), evs.filter((e) => e.repo).length);
  assert.deepEqual(g.actor.items.map((i) => i.value), ['octo-dev']);
  assert.equal(g.actor.items[0].count, byKind('decision').length);
  assert.deepEqual(a.buildFacets([])[0].items, []);
});

test('filters are AND across facets and OR within one', () => {
  const evs = events();
  const f = a.emptyFilters();
  assert.equal(a.applyFilters(evs, f).length, evs.length);
  f.facets.kind = ['fault'];
  assert.equal(a.applyFilters(evs, f).length, 2);
  f.facets.kind = ['fault', 'retry'];
  assert.equal(a.applyFilters(evs, f).length, 3);
  f.facets.result = ['Recovered'];
  assert.deepEqual(a.applyFilters(evs, f).map((e) => e.kind), ['fault']);
  f.facets.repo = ['acme/api'];
  assert.equal(a.applyFilters(evs, f).length, 0);
  f.facets = { kind: [], result: [], repo: [], actor: ['octo-dev'] };
  assert.ok(a.applyFilters(evs, f).every((e) => e.kind === 'decision'));
  assert.ok(a.isFiltered(f));
  assert.ok(!a.isFiltered(a.emptyFilters()));
});

test('search matches repo, runner, job id or job name, ignoring case', () => {
  const evs = events();
  const q = (text) => a.applyFilters(evs, { ...a.emptyFilters(), q: text });
  assert.ok(q('ACME/WEB').length > 0 && q('ACME/WEB').every((e) => e.repo === 'acme/web'));
  assert.ok(q('faber-8990').length >= 1);
  assert.ok(q('9003').length === 0 || q('9003').every((e) => e.jobId === 9003 || e.runner?.includes('9003')));
  assert.ok(q('8990').some((e) => e.jobId === 8990));
  assert.ok(q('UNIT').some((e) => e.job === 'unit'));
  assert.equal(q('nothing-called-this').length, 0);
  assert.equal(q('   ').length, evs.length);
});

test('the query string round-trips', () => {
  const f = a.emptyFilters();
  f.facets.kind = ['fault', 'retry'];
  f.facets.repo = ['acme/web'];
  f.q = 'lint job';
  const s = a.toQuery(f);
  assert.deepEqual(a.parseQuery(s), f);
  assert.equal(a.toQuery(a.emptyFilters()), '');
  assert.deepEqual(a.parseQuery(''), a.emptyFilters());
  assert.deepEqual(a.parseQuery('?kind=fault&q=x').facets.kind, ['fault']);
  // No window: reading is empty and writing is a no-op.
  assert.equal(a.readLocationQuery(), '');
  a.writeLocationQuery('?kind=fault');
});

test('durations read as the table draws them', () => {
  assert.equal(a.fmtDur(34), '34s');
  assert.equal(a.fmtDur(492), '8m 12s');
  assert.equal(a.fmtDur(600), '10m');
  assert.equal(a.fmtDur(10800), '3h');
  assert.equal(a.fmtDur(11700), '3h 15m');
});

test('verdicts and events read as words; an unknown code stays as it is', () => {
  assert.equal(a.verdictText('refuse:event:repository_dispatch'), 'Refused: repository_dispatch is not an allowed event');
  assert.equal(a.verdictText('refuse:actor:octo-bot'), 'Refused: octo-bot is not a trusted actor');
  assert.equal(a.verdictText('refuse:fork'), 'Refused: the change comes from a fork');
  assert.equal(a.verdictText('provision'), 'Queued for admission');
  assert.equal(a.verdictReason('refuse:public_repo'), 'public repositories are not served by this host');
  assert.equal(a.verdictText('refuse:something_new'), 'refuse:something_new');
  assert.equal(a.eventWords('pull_request'), 'Pull request');
  assert.equal(a.eventWords('odd_event'), 'odd_event');
  const refused = byKind('refused').find((e) => e.detail.startsWith('Refused:'));
  assert.equal(refused.detail, 'Refused: repository_dispatch is not an allowed event');
  assert.match(byKind('decision')[0].detail, /^Pull request \| Queued for admission$/);
  assert.match(byKind('bypass')[0].detail, /octo-bot is not a trusted actor/);
});

test('an outcome carries its attempt, so the view can flag a second attempt', () => {
  assert.ok(byKind('outcome').some((e) => e.attempt === 2));
});
