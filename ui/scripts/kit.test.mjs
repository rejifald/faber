// Renders kit parts to markup through Vite's SSR loader (no DOM or test library needed) and reads
// the built CSS, to hold what a type check cannot see: that wrappers reach the control's element,
// and that the focus ring really draws. `npm test` runs it after `npm run build` in CI; the CSS
// test needs dist/ and says so when it is missing.
import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import React from 'react';
import { renderToStaticMarkup as render } from 'react-dom/server';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let kit;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  kit = await server.ssrLoadModule('/src/kit/index.ts');
});
after(() => server?.close());

const h = (...args) => React.createElement(...args);
const button = (html) => /<button[^>]*>/.exec(html)?.[0] ?? '';

test('a Tooltip around a disabled Button hands its trigger props to the button', () => {
  const bare = button(render(h(kit.Button, { disabled: true }, 'Pause')));
  const wrapped = button(render(h(kit.Tooltip, { content: 'Paused by the host' }, h(kit.Button, { disabled: true }, 'Pause'))));
  assert.match(wrapped, /data-disabled/, wrapped);
  assert.notEqual(wrapped, bare, 'the trigger added nothing to the button');
});

test('a Button passes through attributes and keeps its own class', () => {
  const html = button(render(h(kit.Button, { 'data-testid': 'go', id: 'go' }, 'Go')));
  assert.match(html, /data-testid="go"/);
  assert.match(html, /id="go"/);
  assert.match(html, /class="[^"]*min-h-control-md/);
});

// A wrapper such as Tooltip.Trigger merges its own onClick into the kit button's and reads the
// event, so the kit must pass the event on. Markup cannot show this; the handler is called here.
const fakeClick = () => {
  const e = { nativeEvent: { type: 'click' }, defaultPrevented: false };
  e.preventDefault = () => (e.defaultPrevented = true);
  return e;
};
const clickOf = (el) => (el.props.onClick ? el.props.onClick : clickOf(el.props.children));

test('a kit button passes the click event to its handler', () => {
  for (const make of [
    (onClick) => kit.Button({ children: 'Go', onClick }),
    (onClick) => kit.IconButton({ icon: () => null, label: 'Go', onClick }),
  ]) {
    let got;
    const event = fakeClick();
    clickOf(make((e) => (got = e.nativeEvent.type)))(event);
    assert.equal(got, 'click');
  }
});

test('a loading kit button swallows the click', () => {
  let called = false;
  const event = fakeClick();
  clickOf(kit.Button({ children: 'Go', loading: true, onClick: () => (called = true) }))(event);
  assert.equal(called, false);
  assert.equal(event.defaultPrevented, true);
});

test('a loading Button keeps its label and focus, says it is busy and shows a spinner', () => {
  const html = render(h(kit.Button, { loading: true }, 'Reconcile'));
  assert.match(button(html), /aria-busy="true"/);
  assert.doesNotMatch(button(html), /\sdisabled[\s=>]|\sdata-disabled[\s=>]|aria-disabled="true"/);
  assert.match(html, /motion-safe:animate-spin/);
  assert.match(html, />Reconcile</);
});

test('a loading IconButton shows a spinner in place of its icon', () => {
  const html = render(h(kit.IconButton, { icon: () => null, label: 'Reconcile now', loading: true }));
  assert.match(button(html), /aria-busy="true"/);
  assert.match(button(html), /aria-label="Reconcile now"/);
  assert.match(html, /motion-safe:animate-spin/);
});

test('the built CSS draws a solid focus ring', (t) => {
  const assets = join(ui, 'dist/dashboard/assets');
  if (!existsSync(assets)) {
    assert.fail('no dist/dashboard: run `npm run build` before `npm test`');
  }
  const css = readdirSync(assets)
    .filter((f) => f.endsWith('.css'))
    .map((f) => readFileSync(join(assets, f), 'utf8'))
    .join('\n');
  const rule = (cls) => new RegExp(`\\.${cls.replace(/[:]/g, '\\\\:')}:focus-visible\\{([^}]*)\\}`).exec(css)?.[1] ?? '';
  assert.match(rule('focus-visible:outline-solid'), /outline-style:solid/);
  assert.match(css, /\.focus-visible\\:outline-2:focus-visible\{[^}]*outline-width:2px/);
  // The none variant pins the style to none for every state, focus included.
  assert.doesNotMatch(css, /\.outline-none\{/, 'a class list or a comment Tailwind scans uses outline-none');
  t.diagnostic('focus ring: outline-hidden base, solid on focus-visible');
});

test('every animation runs only when the system allows motion', () => {
  const assets = join(ui, 'dist/dashboard/assets');
  const css = readdirSync(assets)
    .filter((f) => f.endsWith('.css'))
    .map((f) => readFileSync(join(assets, f), 'utf8'))
    .join('\n');
  // Tailwind merges every motion-safe rule into one block, so match the spinner anywhere inside it.
  const safe = /@media \(prefers-reduced-motion:no-preference\)\{([^@]*)\}/.exec(css)?.[1] ?? '';
  assert.match(safe, /\.motion-safe\\:animate-spin\{animation:var\(--animate-spin\)/);
  // The class, not the instance: no `animate-*` utility outside the block, the spinner or any other.
  assert.doesNotMatch(css, /(^|\})\.animate-[a-z-]+\{/, 'an unconditional animate-* would ignore reduced motion');
});

test('the Checkbox box is the size of its step\'s icon, the tick at Lucide\'s own stroke', () => {
  for (const [size, box] of [['sm', 'size-icon-sm'], ['md', 'size-icon-md'], ['lg', 'size-icon-lg']]) {
    const html = render(h(kit.Checkbox, { size, label: 'Keep awake', defaultChecked: true }));
    const boxClass = /<span[^>]*role="checkbox"[^>]*class="([^"]*)"/.exec(html)?.[1].split(' ') ?? [];
    assert.ok(boxClass.includes(box) && boxClass.includes('border-border-control'), `${size}: ${boxClass.join(' ')}`);
    assert.doesNotMatch(html, /stroke-width="3"/, size);
  }
});

test('the theme defines the spinner animation', () => {
  const css = readFileSync(join(ui, 'src/theme/theme.css'), 'utf8');
  assert.match(css, /--animate-spin:/);
  assert.match(css, /@keyframes spin/);
});

test('a ring StatusDot is a stroked, unfilled circle, and wins over pulse', () => {
  const html = render(h(kit.StatusDot, { tone: 'amber', ring: true, pulse: true }));
  const circles = html.match(/<circle[^>]*>/g) ?? [];
  assert.equal(circles.length, 1, 'no pulse ring beside it');
  assert.match(circles[0], /fill="none"/);
  assert.match(circles[0], /stroke-amber/);
  assert.doesNotMatch(circles[0], /class="[^"]*\bfill-amber/);
  const solid = render(h(kit.StatusDot, { tone: 'amber' }));
  assert.match(solid, /fill-amber/);
  assert.doesNotMatch(solid, /fill="none"/);
});
