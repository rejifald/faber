// The theme reads its colours from desktop/src/tokens.css (ADR-0037), whose values
// tests/design_tokens.rs holds to WCAG contrast. What the theme adds is the mapping from each
// role to a utility, so this proves the mapping is complete and points only at real roles: a role
// missing here has no utility, and a utility pointing at a missing role renders nothing.
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

const ui = join(import.meta.dirname, '..');
const tokens = readFileSync(join(ui, '../desktop/src/tokens.css'), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
const theme = readFileSync(join(ui, 'src/theme/theme.css'), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');

/** Custom properties declared in `css` whose value is a colour, in declaration order. */
const colourRoles = (css) => [
  ...new Set(
    [...css.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)]
      .filter(([, , value]) => /^(#|rgba?\(|hsla?\(|oklch\()/i.test(value.trim()))
      .map(([, name]) => name.slice(2)),
  ),
];

const mapped = new Map(
  [...theme.matchAll(/--color-([\w-]+)\s*:\s*var\(--([\w-]+)\)\s*;/g)].map(([, utility, role]) => [utility, role]),
);

test('every colour role in tokens.css has a utility of the same name', () => {
  const roles = colourRoles(tokens);
  assert.ok(roles.includes('accent-text'), 'expected the token file to carry --accent-text');
  for (const role of roles) assert.equal(mapped.get(role), role, `--color-${role}: var(--${role}) missing from theme.css`);
});

test('every colour utility reads a role tokens.css defines', () => {
  const roles = new Set(colourRoles(tokens));
  for (const [utility, role] of mapped) assert.ok(roles.has(role), `--color-${utility} reads --${role}, which tokens.css does not define`);
});

// The scales and the ladder, held to the ADRs' own tables, so a value cannot change in the theme
// or the kit without the ADR that owns it.
const adr = (n) => {
  const dir = join(ui, '../docs/adrs');
  const file = readdirSync(dir).find((f) => f.startsWith(`${n}-`));
  return readFileSync(join(dir, file), 'utf8');
};
/** `{ name: px }` from the theme's `--<namespace>-<name>: <px>px;` declarations. */
const themeSteps = (namespace) =>
  Object.fromEntries([...theme.matchAll(new RegExp(`--${namespace}-([\\w-]+):\\s*(\\d+)px;`, 'g'))].map(([, k, v]) => [k, Number(v)]));
/** `{ name: px }` from an ADR table's `| \`--<prefix>-<name>\` | <px> |` rows. */
const adrSteps = (text, prefix) =>
  Object.fromEntries([...text.matchAll(new RegExp(`^\\s*\\| \`--${prefix}-([\\w-]+)\` \\| (\\d+) \\|`, 'gm'))].map(([, k, v]) => [k, Number(v)]));

test('spacing, radius and type are exactly ADR-0038 tables', () => {
  const text = adr('0038');
  const spacing = adrSteps(text, 'space');
  assert.equal(Object.keys(spacing).length, 10, 'ADR-0038 spacing table');
  const themeSpacing = themeSteps('spacing');
  for (const [name, px] of Object.entries(spacing)) assert.equal(themeSpacing[name], px, `--spacing-${name}`);
  const extra = Object.keys(themeSpacing).filter((k) => !(k in spacing) && k !== '0' && !/^(control|icon)-/.test(k));
  assert.deepEqual(extra, [], 'spacing steps the ADR does not have');

  const radius = adrSteps(text, 'radius');
  assert.equal(radius.xs, 4, 'ADR-0038 radius-xs (amended 2026-10-18)');
  assert.deepEqual(themeSteps('radius'), radius);

  const type = adrSteps(text, 'text');
  assert.equal(Object.keys(type).length, 5, 'ADR-0038 type table');
  assert.deepEqual(themeSteps('text'), type);
});

test('the kit ladder is exactly ADR-0039 § 1', () => {
  const rows = [...adr('0039').matchAll(/^\s*\| `(sm|md|lg)` \| (\d+) \| `--space-([\w-]+)` \(\d+\) \| `--text-([\w-]+)` \((\d+)\) \| (\d+) \| `--space-([\w-]+)` \(\d+\) \| `--radius-([\w-]+)` \|/gm)];
  assert.equal(rows.length, 3, 'ADR-0039 ladder table');
  const ladderTs = readFileSync(join(ui, 'src/kit/ladder.ts'), 'utf8');
  const block = /export const ladder[^{]*\{([\s\S]*?)\n\};/.exec(ladderTs)[1];
  const classes = Object.fromEntries([...block.matchAll(/^\s+(sm|md|lg): '([^']+)'/gm)].map(([, k, v]) => [k, v.split(' ')]));
  const squareBlock = /export const square[^{]*\{([\s\S]*?)\n\};/.exec(ladderTs)[1];
  const squares = Object.fromEntries([...squareBlock.matchAll(/^\s+(sm|md|lg): '([^']+)'/gm)].map(([, k, v]) => [k, v.split(' ')]));
  // A height and an icon are `calc(<px>em / <label px>)`: the table's px at the step's own
  // label, and scaling with it (ADR-0039 § 1, amended 2026-10-18).
  const relative = (kind) =>
    Object.fromEntries(
      [...theme.matchAll(new RegExp(`--spacing-${kind}-(sm|md|lg):\\s*calc\\((\\d+)em \\/ (\\d+)\\);`, 'g'))].map(([, k, px, per]) => [k, [Number(px), Number(per)]]),
    );
  const heights = relative('control');
  const icons = relative('icon');
  for (const [, step, height, pad, label, labelPx, icon, gap, radius] of rows) {
    assert.deepEqual(heights[step], [Number(height), Number(labelPx)], `${step} height`);
    assert.deepEqual(icons[step], [Number(icon), Number(labelPx)], `${step} icon`);
    assert.deepEqual(
      [...classes[step]].sort(),
      [`min-h-control-${step}`, `px-${pad}`, `gap-${gap}`, `text-${label}`, `rounded-${radius}`].sort(),
      `ladder.${step}`,
    );
    // The square takes the same minimum height, as its width too, so it lines up with a
    // labelled control of its step at every text size.
    assert.deepEqual(
      [...squares[step]].sort(),
      [`min-h-control-${step}`, `min-w-control-${step}`, 'justify-center', `text-${label}`, `rounded-${radius}`].sort(),
      `square.${step}`,
    );
  }
});

test('the boxed shape (Checkbox) takes each step\'s label and gap from ADR-0039, and no height', () => {
  const rows = [...adr('0039').matchAll(/^\s*\| `(sm|md|lg)` \| \d+ \| `--space-[\w-]+` \(\d+\) \| `--text-([\w-]+)` \(\d+\) \| \d+ \| `--space-([\w-]+)` \(\d+\) \|/gm)];
  assert.equal(rows.length, 3, 'ADR-0039 ladder table');
  const ladderTs = readFileSync(join(ui, 'src/kit/ladder.ts'), 'utf8');
  const block = /export const boxed[^{]*\{([\s\S]*?)\n\};/.exec(ladderTs)[1];
  const classes = Object.fromEntries([...block.matchAll(/^\s+(sm|md|lg): '([^']+)'/gm)].map(([, k, v]) => [k, v.split(' ')]));
  for (const [, step, label, gap] of rows) {
    assert.deepEqual([...classes[step]].sort(), [`gap-${gap}`, `text-${label}`].sort(), `boxed.${step}`);
  }
  assert.match(adr('0039'), /`boxed`/, 'ADR-0039 records the boxed shape');
});
