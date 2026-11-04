// The Settings view's data logic (ui/src/settings/), held to settings/anatomy.md § 5 "PR 2: data and
// shared logic": the blank, optional and unstated rules ported from desktop/src/settings.js, the
// live/restart tagging, sudo quoting, narrowing, the list rules and search. Loaded through Vite's SSR loader.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let draft;
let lists;
let search;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  draft = await server.ssrLoadModule('/src/settings/draft.ts');
  lists = await server.ssrLoadModule('/src/settings/lists.ts');
  search = await server.ssrLoadModule('/src/settings/search.ts');
});
after(() => server?.close());

const f = (kind, extra = {}) => ({ key: 'a.b', label: 'Label', kind, live: false, optional: false, ...extra });

test('dig and plant walk dotted keys', () => {
  const o = {};
  draft.plant(o, 'a.b.c', 3);
  assert.deepEqual(o, { a: { b: { c: 3 } } });
  assert.equal(draft.dig(o, 'a.b.c'), 3);
  assert.equal(draft.dig(o, 'a.x.c'), undefined);
});

test('readValue: a held field round-trips as loaded', () => {
  assert.equal(draft.readValue(f('bool', { held: true }), false, true), true);
  assert.equal(draft.readValue(f('text', { held: true }), 'changed', 'kept'), 'kept');
});

test('readValue: a sudo field round-trips as loaded', () => {
  assert.deepEqual(draft.readValue(f('lines', { sudo: 'sudo x' }), 'changed', ['kept']), ['kept']);
});

test('readValue: bool, required and optional tri-state', () => {
  assert.equal(draft.readValue(f('bool'), true, null), true);
  assert.equal(draft.readValue(f('bool'), false, null), false);
  const opt = f('bool', { optional: true });
  assert.equal(draft.readValue(opt, '', true), null);
  assert.equal(draft.readValue(opt, 'true', null), true);
  assert.equal(draft.readValue(opt, 'false', null), false);
  assert.equal(draft.readValue(f('bool', { optional: true, unstated_saves: 0 }), '', null), false);
  assert.equal(draft.readValue(f('bool', { optional: true, unstated_saves: 1 }), '', null), true);
});

test('readValue: enum and text, blank optional is null, blank required is empty', () => {
  assert.equal(draft.readValue(f('enum'), 'docker', null), 'docker');
  assert.equal(draft.readValue(f('text'), '  hi ', null), 'hi');
  assert.equal(draft.readValue(f('text', { optional: true }), '  ', 'x'), null);
  assert.equal(draft.readValue(f('text'), '', 'x'), '');
});

test('readValue: ceiling is auto, off, or a positive number, else off', () => {
  const c = f('ceiling');
  assert.equal(draft.readValue(c, 'auto', null), 'auto');
  assert.equal(draft.readValue(c, 'off', null), 'off');
  assert.equal(draft.readValue(c, '4.4', null), 4);
  assert.equal(draft.readValue(c, '0', null), 'off');
  assert.equal(draft.readValue(c, '', null), 'off');
  assert.equal(draft.readValue(c, 'x', null), 'off');
});

test('readValue: csv, lines and map drop blanks and trim', () => {
  assert.deepEqual(draft.readValue(f('csv'), ' a, b ,,c', null), ['a', 'b', 'c']);
  assert.deepEqual(draft.readValue(f('csv'), '', null), []);
  assert.deepEqual(draft.readValue(f('lines'), 'a\n\n b \n', null), ['a', 'b']);
  assert.deepEqual(draft.readValue(f('map'), 'K=v=w\n bare \n\n X = 1 ', null), { K: 'v=w', bare: '', X: '1' });
});

test('readValue: blank numbers are unset, unstated_saves, or the default', () => {
  const defaults = { a: { b: 7 } };
  assert.equal(draft.readValue(f('int'), '', 1, defaults), 7);
  assert.equal(draft.readValue(f('int', { optional: true }), '', 1, defaults), null);
  assert.equal(draft.readValue(f('int', { optional: true, unstated_saves: 0 }), '', null, defaults), 0);
  assert.equal(draft.readValue(f('int'), 'abc', 1, defaults), 7);
  assert.equal(draft.readValue(f('int'), '2.6', 1, defaults), 3);
  assert.equal(draft.readValue(f('float'), '1.25', 1, defaults), 1.25);
  assert.equal(draft.readValue(f('int'), '5', 1), 5);
});

test('asText is the inverse shape of readValue', () => {
  assert.equal(draft.asText(f('csv'), ['a', 'b']), 'a, b');
  assert.equal(draft.asText(f('lines'), ['a', 'b']), 'a\nb');
  assert.equal(draft.asText(f('map'), { K: 'v' }), 'K=v');
  assert.equal(draft.asText(f('int'), null), '');
});

test('changes lists changed keys tagged live or restart', () => {
  const schema = {
    sections: [
      { title: 'S', fields: [f('int', { key: 'autolimit.margin', label: 'Margin', live: true }), f('text', { key: 'topic', label: 'Topic' })] },
    ],
    profile_fields: [f('text', { key: 'name', label: 'Name' })],
  };
  const loaded = { autolimit: { margin: 1.2 }, topic: 'faber', profile: [{ name: 'a' }, { name: 'b' }] };
  assert.deepEqual(draft.changes(structuredClone(loaded), loaded, schema), []);
  const d = structuredClone(loaded);
  d.autolimit.margin = 1.3;
  d.topic = 'x';
  d.profile[1].name = 'c';
  assert.deepEqual(
    draft.changes(d, loaded, schema).map((c) => [c.key, c.effect]),
    [['autolimit.margin', 'live'], ['topic', 'restart'], ['profile.1.name', 'restart']],
  );
  d.profile.push({ name: 'n' });
  assert.ok(draft.changes(d, loaded, schema).some((c) => c.key === 'profile' && c.effect === 'restart'));
});

test('sudoCmd fills and quotes the profile name as one shell word', () => {
  const tpl = 'sudo faber config set profile.<profile>.dind true';
  assert.equal(draft.sudoCmd(tpl, { name: 'linux-docker' }), 'sudo faber config set profile.linux-docker.dind true');
  assert.equal(draft.sudoCmd(tpl, { name: 'my box' }), "sudo faber config set 'profile.my box.dind' true");
  assert.equal(draft.sudoCmd(tpl, { name: "it's" }), "sudo faber config set 'profile.it'\\''s.dind' true");
  assert.equal(draft.sudoCmd(tpl, null), tpl);
  assert.equal(draft.shq(''), "''");
});

test('narrowShown: numbers only when below, others when they differ', () => {
  const num = f('int', { narrow: { label: 'Cap', value: '3' } });
  assert.equal(draft.narrowShown(num, 2), true);
  assert.equal(draft.narrowShown(num, 3), false);
  assert.equal(draft.narrowShown(num, null), false);
  const bool = f('bool', { narrow: { label: 'Turn off', value: 'false' } });
  assert.equal(draft.narrowShown(bool, true), true);
  assert.equal(draft.narrowShown(bool, false), false);
  assert.equal(draft.narrowShown(bool, null), true);
  const list = f('lines', { narrow: { label: 'Clear', value: '[]' } });
  assert.equal(draft.narrowShown(list, ['a']), true);
  assert.equal(draft.narrowShown(list, []), false);
  assert.equal(draft.narrowShown(list, null), false);
  assert.equal(draft.narrowShown(f('text'), 'x'), false);
});

test('lists: canAdd and canRemove per narrows mode', () => {
  const mk = (narrows) => f('lines', { sudo: 'sudo x', narrows });
  assert.equal(lists.canAdd(mk('add')), true);
  assert.equal(lists.canAdd(mk('remove')), false);
  assert.equal(lists.canAdd(mk('remove-keep-one')), false);
  assert.equal(lists.canAdd(mk(undefined)), false);
  assert.equal(lists.canRemove(mk('remove'), 0, 1), true);
  assert.equal(lists.canRemove(mk('add'), 0, 3), false);
  assert.equal(lists.canRemove(mk(undefined), 0, 3), false);
  assert.equal(lists.canRemove(mk('remove-keep-one'), 0, 2), true);
  assert.equal(lists.canRemove(mk('remove-keep-one'), 0, 1), false, 'the last item stays');
  assert.equal(lists.canAdd(f('lines')), true);
  assert.equal(lists.canRemove(f('lines'), 0, 1), true);
});

test('validateItem: pattern first, then duplicate', () => {
  const field = f('lines', { item: { pattern: '[A-Za-z0-9-]+/[A-Za-z0-9._-]+', example: 'owner/repo', error: 'Use owner/repo.' } });
  assert.equal(lists.validateItem(field, 'owner/repo', []), null);
  assert.equal(lists.validateItem(field, 'nope', ['nope']), 'Use owner/repo.');
  assert.equal(lists.validateItem(field, 'a/b extra', []), 'Use owner/repo.');
  assert.match(lists.validateItem(field, 'a/b', ['a/b']), /already/);
  assert.equal(lists.validateItem(f('lines'), 'anything', ['x']), null);
});

test('search matches label, key and hint, case-insensitively', () => {
  const schema = {
    sections: [
      { title: 'Limits', fields: [f('int', { key: 'autolimit.margin', label: 'Safety margin', hint: 'Multiplier on a job' })] },
      { title: 'Daemon', fields: [f('text', { key: 'topic', label: 'Topic' })] },
    ],
    profile_fields: [],
  };
  const keys = (q) => search.matches(q, schema).map((h) => `${h.section.title}>${h.field.key}`);
  assert.deepEqual(keys('SAFETY'), ['Limits>autolimit.margin']);
  assert.deepEqual(keys('autolimit'), ['Limits>autolimit.margin']);
  assert.deepEqual(keys('multiplier'), ['Limits>autolimit.margin']);
  assert.deepEqual(keys('topic'), ['Daemon>topic']);
  assert.deepEqual(keys('zzz'), []);
  assert.deepEqual(keys('  '), []);
});

test('changedLines marks the edited and inserted lines, one-based', async () => {
  const raw = await server.ssrLoadModule('/src/settings/raw.ts');
  assert.deepEqual(raw.changedLines('a\nb\nc', 'a\nb\nc'), []);
  assert.deepEqual(raw.changedLines('a\nB\nc', 'a\nb\nc'), [2]);
  assert.deepEqual(raw.changedLines('a\nnew\nb\nc', 'a\nb\nc'), [2]);
  assert.deepEqual(raw.changedLines('a\nc', 'a\nb\nc'), []);
  assert.deepEqual(raw.changedLines('x\ny', ''), [1, 2]);
});

test('nav maps daemon sections to groups, unknown last in faber, Profiles and raw added', async () => {
  const nav = await server.ssrLoadModule('/src/settings/nav.ts');
  const section = (title, live = false) => ({ title, fields: [{ key: 'k', label: 'K', kind: 'text', live, optional: false }] });
  const schema = {
    sections: [section('Updates', true), section('Brand new'), section('Trust'), section('Autolimiter', true), section('Daemon')],
    profile_fields: [],
  };
  const items = nav.navItems(schema, 3, true);
  assert.deepEqual(
    items.map((i) => `${i.group}:${i.title}`),
    ['Resources:Limits', 'Jobs:Trust', 'Jobs:Profiles', 'faber:Daemon', 'faber:Updates', 'faber:Brand new', 'faber:config.toml'],
  );
  assert.equal(items.find((i) => i.title === 'Profiles').tag, '3');
  assert.equal(items.find((i) => i.title === 'Limits').tag, 'live');
  assert.equal(items.find((i) => i.title === 'Trust').tag, undefined);
  assert.equal(nav.sectionId('Runner-image drift'), 'runner-image-drift');
  assert.equal(nav.groupOf('Nothing known'), 'faber');
  assert.deepEqual(
    nav.groupedNav(items).map((g) => g.group),
    ['Resources', 'Jobs', 'faber'],
  );
  assert.deepEqual(nav.groupedNav(nav.navItems({ sections: [], profile_fields: [] }, 0)).map((g) => g.group), ['Jobs']);
});

test('profileFacts: backend, Docker access unless stated no, VM cores and memory', async () => {
  const { profileFacts } = await server.ssrLoadModule('/src/blocks/ProfileRow.tsx');
  assert.deepEqual(profileFacts({ backend: 'docker', dind: true }), ['docker', 'Docker access']);
  assert.deepEqual(profileFacts({ backend: 'docker' }), ['docker', 'Docker access']);
  assert.deepEqual(profileFacts({ backend: 'docker', dind: false }), ['docker']);
  assert.deepEqual(profileFacts({ backend: 'macos-vm', cpus: 4, memory_mib: 8192 }), ['macos-vm', '4 vCPU', '8 GiB']);
  assert.deepEqual(profileFacts({ backend: 'macos-vm', memory_mib: 12800 }), ['macos-vm', '12.5 GiB']);
});

test('trustText words a value for a trust row', async () => {
  const { trustText } = await server.ssrLoadModule('/src/blocks/TrustRow.tsx');
  assert.equal(trustText(f('bool'), true), 'on');
  assert.equal(trustText(f('bool', { options: ['unset', 'yes', 'no'] }), false), 'no');
  assert.equal(trustText(f('bool', { optional: true, options: ['not stated', 'yes', 'no'] }), null), 'not stated');
  assert.equal(trustText(f('csv'), ['faber', 'docker']), 'faber, docker');
  assert.equal(trustText(f('csv'), []), 'none');
  assert.equal(trustText(f('map'), { A: '1', B: '2' }), 'A=1, B=2');
  assert.equal(trustText(f('text'), null), 'not set');
});

test('the Autolimiter is two nav items: Limits (live fields, tagged) and Admission tuning (the rest)', async () => {
  const nav = await server.ssrLoadModule('/src/settings/nav.ts');
  const field = (key, live) => ({ key, label: key, kind: 'int', live, optional: false });
  const schema = {
    sections: [{ title: 'Autolimiter', hint: 'H', fields: [field('a', true), field('b', false), field('c', true)] }, { title: 'Docker host', fields: [field('d', false)] }],
    profile_fields: [],
  };
  const items = nav.navItems(schema, 0, true);
  assert.deepEqual(items.map((i) => i.title), ['Limits', 'Admission tuning', 'Docker host', 'Profiles', 'config.toml']);
  assert.equal(items[0].tag, 'live');
  assert.equal(items[1].tag, undefined);
  const sections = nav.viewSections(schema);
  assert.deepEqual(sections[0].fields.map((f) => f.key), ['a', 'c']);
  assert.deepEqual(sections[1].fields.map((f) => f.key), ['b']);
  assert.equal(items.at(-1).id, nav.RAW_ID);
});
