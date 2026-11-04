// Proves each rule of eslint.config.mjs fires where it should and stays quiet where it should not,
// by linting snippets as if they lived at a given path. Runs the real config, so a change to the
// config's scopes is tested too. `npm test` runs it.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { test } from 'node:test';
import { ESLint } from 'eslint';

const eslint = new ESLint({ cwd: join(import.meta.dirname, '..') });

/** The rule ids `code` breaks when it sits at `path` (relative to ui/). */
async function broken(path, code) {
  const [result] = await eslint.lintText(code, { filePath: path });
  return result.messages.map((m) => m.ruleId);
}

const SCREEN = 'src/screens/Overview.tsx';
const KIT = 'src/kit/Thing.tsx';

test('arbitrary values are rejected, in screens and in the kit', async () => {
  for (const cls of ['p-[7px]', 'bg-[#fff]', 'hover:w-[calc(100%-2px)]', '[mask-type:luminance]', '[&>svg]:hidden', 'h-(--x)', 'text-(length:--x)', 'bg-(color:--brand)']) {
    const code = `export const A = () => <div className="flex ${cls}" />;`;
    assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-arbitrary-class'], cls);
    assert.deepEqual(await broken(KIT, code), ['design-tokens/no-arbitrary-class'], cls);
  }
});

test('an arbitrary class is caught beside a word that is not a class', async () => {
  const code = 'export const a = "flex Gap p-[7px]";';
  assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-arbitrary-class']);
});

test('an arbitrary value split by a template expression is caught', async () => {
  const code = 'export const a = (x: number) => `flex p-[${x}px]`;';
  assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-arbitrary-class']);
});

test('theme steps, prose and code in strings pass', async () => {
  const code = [
    'export const a = "flex gap-sm p-xl bg-surface text-fg rounded-md data-active:bg-surface";',
    'export const b = "Pick a value [optional]";',
    'export const c = "items[0]";',
    'export const d = "Pick a value [optional] or see run #42";',
    "export const tone = 'red';",
  ].join('\n');
  assert.deepEqual(await broken(SCREEN, code), []);
});

test('raw colours are rejected', async () => {
  for (const colour of ['#fff', '#0b0e12', 'rgb(0, 0, 0)', 'oklch(0.7 0.1 30)']) {
    const code = `export const c = '${colour}';`;
    assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-raw-colour'], colour);
  }
});

test('a colour inside a longer value is rejected', async () => {
  for (const value of ['1px solid #ccc', '0 0 0 1px #0b0e12aa', 'linear-gradient(#fff, #000)', '0 1px 2px #000', '#000']) {
    const code = `export const c = '${value}';`;
    assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-raw-colour'], value);
  }
});

test('a digit-only hex is a colour where a colour is expected', async () => {
  for (const code of [
    "export const s = { boxShadow: '0 0 #000' };",
    "export const s = { background: '#000 url(x.png)' };",
    "export const s = { outline: 'medium double #999' };",
    'export const s = (w: number) => ({ boxShadow: `0 0 0 ${w}px #000` });',
    'export const A = () => <svg fill="#000" stroke={`#111`} />;',
  ]) {
    const expected = code.includes('<svg') ? ['design-tokens/no-raw-colour', 'design-tokens/no-raw-colour'] : ['design-tokens/no-raw-colour'];
    assert.deepEqual(await broken(KIT, code), expected, code);
  }
});

test('issue and run numbers in copy are not colours', async () => {
  const code = [
    'export const a = "Fixed in issue #280";',
    'export const b = "See run #1234 for the log";',
    'export const c = `Job #123 of PR #45678901`;',
    'export const d = "Run #1234 failed at 10%";',
    'export const e = { title: "Retry run #1234" };',
  ].join('\n');
  assert.deepEqual(await broken(SCREEN, code), []);
});

test('named colours are rejected where a colour is expected', async () => {
  assert.deepEqual(await broken(KIT, "export const s = { color: 'red' };"), ['design-tokens/no-raw-colour']);
  assert.deepEqual(await broken(KIT, "export const s = { borderTop: '1px solid tomato' };"), ['design-tokens/no-raw-colour']);
  assert.deepEqual(await broken(KIT, 'export const A = () => <svg fill="black" />;'), ['design-tokens/no-raw-colour']);
  assert.deepEqual(await broken(KIT, 'export const A = () => <svg stroke={"white"} />;'), ['design-tokens/no-raw-colour']);
  assert.deepEqual(await broken(KIT, 'export const A = () => <svg fill="currentColor" stroke="none" />;'), []);
});

test('Base UI is imported only in the kit', async () => {
  const code = "import { Button } from '@base-ui/react/button';\nexport { Button };";
  assert.deepEqual(await broken(SCREEN, code), ['no-restricted-imports']);
  assert.deepEqual(await broken('src/entries/x.tsx', code), ['no-restricted-imports']);
  assert.deepEqual(await broken(KIT, code), []);
});

test('a dynamic import of Base UI is rejected outside the kit', async () => {
  const code = "export const load = () => import('@base-ui/react/menu');";
  assert.deepEqual(await broken(SCREEN, code), ['no-restricted-syntax']);
  assert.deepEqual(await broken('src/lib/lazy.ts', code), ['no-restricted-syntax']);
  assert.deepEqual(await broken(KIT, code), []);
});

test('raw controls are rendered only in the kit', async () => {
  const code = 'export const A = () => <button type="button">Go</button>;';
  assert.deepEqual(await broken(SCREEN, code), ['no-restricted-syntax']);
  assert.deepEqual(await broken(KIT, code), []);
});

test('nothing outside the kit takes a style prop', async () => {
  const code = 'export const A = () => <div style={{ opacity: 0.5 }} />;';
  for (const path of [SCREEN, 'dashboard/main.tsx', 'src/blocks/Row.tsx', 'src/lib/util.tsx']) {
    assert.deepEqual(await broken(path, code), ['no-restricted-syntax'], path);
  }
  assert.deepEqual(await broken(KIT, code), []);
});

test('raw sizes in a style object are rejected everywhere', async () => {
  const code = "export const s = { padding: '7px', fontSize: 13, maxWidth: 480, top: '4px', inset: 2 };";
  assert.deepEqual(await broken(KIT, code), Array(5).fill('design-tokens/no-raw-style-number'));
});

test('ladder sizes stay in the kit, and only as sizes', async () => {
  const sized = 'export const a = "h-control-md size-icon-sm min-w-control-lg";';
  assert.deepEqual(await broken(KIT, sized), []);
  assert.deepEqual(await broken(SCREEN, sized), ['design-tokens/ladder-size']);
  for (const cls of ['p-control-lg', 'gap-icon-sm', 'hover:px-control-md', 'mt-icon-lg']) {
    const code = `export const a = "flex ${cls}";`;
    assert.deepEqual(await broken(KIT, code), ['design-tokens/ladder-size'], cls);
  }
});

test('a middle dot or bullet between facts is rejected in ui/src, in strings, templates and JSX text', async () => {
  const dot = '\u00b7';
  const bullet = '\u2022';
  for (const code of [
    `export const a = "3 jobs ${dot} 2 runners";`,
    `export const a = (n: number) => \`${dot} \${n} queued\`;`,
    `export const A = () => <p>Paused ${bullet} 2 queued</p>;`,
  ]) {
    assert.deepEqual(await broken(SCREEN, code), ['design-tokens/no-dot-separator'], code);
    assert.deepEqual(await broken(KIT, code), ['design-tokens/no-dot-separator'], code);
  }
});

test('one phrase and the spaced pipe pass the separator rule', async () => {
  const code = 'export const A = () => <p>3 jobs, 2 runners <span aria-hidden className="text-faint">|</span> v0.4.0</p>;';
  assert.deepEqual(await broken(SCREEN, code), []);
});
