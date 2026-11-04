// node:test suite for catalog.mjs (copied with it from a design-token lint pack). It builds a
// throwaway fixture repo, so it needs no project components. `npm test` runs it.
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { after, test } from 'node:test';
import {
  collectEntries,
  collectRoutes,
  globToRegExp,
  parseDoc,
  parseProps,
  problems,
  render,
  storyProblems,
} from './catalog.mjs';

const root = mkdtempSync(join(tmpdir(), 'catalog-'));
after(() => rmSync(root, { recursive: true, force: true }));

const put = (path, text) => {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), text);
};

put(
  'kit/Button.tsx',
  `interface ButtonProps {
  /** Visible label. */
  label: string;
  /** Called on press. */
  onPress?: (id: string) => void;
}

/**
 * A pressable action.
 *
 * @useWhen a single action; use Link for navigation.
 * @states default, pressed, disabled
 */
export function Button(props: ButtonProps) {}

/**
 * Reads the theme.
 *
 * @useWhen colours are needed.
 */
export function useTheme() {}
`,
);
put('kit/Button.stories.tsx', 'export const Default = {};\nexport const Disabled = {};\n');
put(
  'kit/Old.tsx',
  `/**
 * Old thing.
 *
 * @useWhen never.
 * @legacy replaced by Button
 */
export function OldThing() {}
`,
);
put('kit/Old.stories.tsx', 'export const Default = {};\n');
put('kit/Bare.tsx', 'export function Bare(props: BareProps) {}\ninterface BareProps {\n  size: number;\n}\n');
put('app/_layout.tsx', 'export default function Layout() {}\n');
put('app/(tabs)/home.tsx', 'export default function Home() {}\n');
put('app/item/[id].tsx', 'export default function Item() {}\n');
put('app/old.tsx', '// @route-alias\nexport default function Old() {}\n');
put('stories/home.stories.tsx', 'export const Default = {};\n');

const config = {
  output: 'catalog.md',
  groups: [
    {
      name: 'Kit',
      type: 'components',
      sources: ['kit/*.tsx'],
      exclude: ['**/*.stories.tsx'],
      story: '{dir}/{file}.stories.tsx',
    },
    { name: 'Legacy', type: 'legacy' },
    {
      name: 'Screens',
      type: 'routes',
      routesDir: 'app',
      sources: ['app/**/*.tsx'],
      story: 'stories/{key}.stories.tsx',
    },
  ],
};
const ctx = { config, root };
const entries = collectEntries(ctx);
const routes = collectRoutes(ctx);

test('globs support **, * and {a,b}', () => {
  assert.ok(globToRegExp('a/**/*.{ts,tsx}').test('a/b/c/d.tsx'));
  assert.ok(globToRegExp('a/**/*.ts').test('a/d.ts'));
  assert.ok(!globToRegExp('a/*.ts').test('a/b/d.ts'));
});

test('docstrings split into description and tags', () => {
  const doc = parseDoc('/**\n * What.\n * More.\n *\n * @useWhen now\n *   and later.\n * @states a, b\n */');
  assert.equal(doc.what, 'What. More.');
  assert.deepEqual(doc.tags, { useWhen: 'now and later.', states: 'a, b' });
});

test('props are read from the props interface with their docs', () => {
  const props = parseProps(
    'type XProps = {\n  /** A. */\n  a?: string | number;\n  b: (x: number) => void;\n};',
    'XProps',
  );
  assert.deepEqual(
    props.map((p) => [p.name, p.optional, p.doc]),
    [
      ['a', true, 'A.'],
      ['b', false, ''],
    ],
  );
});

test('components are found and sorted into their group; hooks and legacy are recognised', () => {
  assert.deepEqual(entries.filter((e) => e.group === 'Kit').map((e) => e.name), ['Bare', 'Button', 'useTheme']);
  assert.deepEqual(entries.filter((e) => e.group === 'Legacy').map((e) => e.name), ['OldThing']);
  assert.equal(entries.find((e) => e.name === 'Button').stories, 2);
});

test('missing docstring parts are reported, with the file', () => {
  const found = problems(entries).join('\n');
  assert.match(found, /Bare \(kit\/Bare\.tsx\): no description/);
  assert.match(found, /Bare .*no @useWhen/);
  assert.match(found, /Bare .*no @states/);
  assert.match(found, /Bare .*prop "size" has no doc/);
  assert.doesNotMatch(found, /^(Button|useTheme|OldThing) /m);
});

test('routes skip layouts and aliases, drop route groups and params from the key', () => {
  assert.deepEqual(routes.map((r) => r.key), ['home', 'item']);
});

test('a component or route without a story fails the coverage check', () => {
  const found = storyProblems(entries, routes, undefined, root).join('\n');
  assert.match(found, /Bare \(kit\/Bare\.tsx\): no stories in kit\/Bare\.stories\.tsx/);
  assert.match(found, /app\/item\/\[id\]\.tsx: no stories in stories\/item\.stories\.tsx/);
  assert.doesNotMatch(found, /^Button |home\.tsx/m);
});

test('hooks need no stories, and requireStory: false opts a group out', () => {
  assert.deepEqual(storyProblems(entries.filter((e) => e.hook), [], () => 0), []);
  assert.deepEqual(storyProblems([], routes.map((r) => ({ ...r, requireStory: false })), () => 0), []);
});

test('render is deterministic and lists every group', () => {
  const a = render(entries, routes, config);
  assert.equal(a, render(entries, routes, config));
  for (const heading of ['## Kit', '## Legacy', '## Screens', '### Button']) assert.ok(a.includes(heading), heading);
});
