// faber's UI lint (ADR-0042 § 8), from a design-token eslint pack adapted to
// Tailwind. What it holds, by scope:
//
//   everything      no raw colours, no Tailwind arbitrary values, no raw sizes in style objects,
//                   no Base UI (static or dynamic import), no raw <button>/<input>/<select>/
//                   <textarea>, no `style` prop: outside the kit, code composes kit parts
//                   no control-ladder size (`h-control-md`, `size-icon-sm`): a screen picks `size`
//   ui/src/         no middle dot or bullet between facts (the separator rule, guidelines § Copy)
//   ui/src/kit/     the one place Base UI and the raw elements are used, `style` is allowed, and
//                   ladder sizes are, but only on h, w, size, min-* and max-*, never as spacing
//   ui/src/theme/   holds the values, so none of the above applies (it holds no TS today)
//
// A genuine one-off is an inline disable with its reason on the same line, never a file-wide or
// bare disable:
//   <svg fill="#000"> // eslint-disable-line design-tokens/no-raw-colour -- the logo mark's own ink

import js from '@eslint/js';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';
import designTokens from './eslint-rules/design-tokens.mjs';

const guidelines = (text) => `${text} (docs/design/guidelines.md, ADR-0042)`;

const code = ['**/*.{ts,tsx}'];
const kit = ['src/kit/**/*.{ts,tsx}'];
const exempt = ['src/theme/**', 'vite.config.ts'];

const baseUi = {
  group: ['@base-ui/react', '@base-ui/react/*'],
  message: guidelines('Base UI is wrapped once, in ui/src/kit: import the kit part instead.'),
};

const rawElement = (tag, part) => ({
  selector: `JSXOpeningElement[name.name='${tag}']`,
  message: guidelines(`No raw <${tag}>: use the kit's ${part}.`),
});
const rawElements = [
  rawElement('button', 'Button or IconButton'),
  rawElement('input', 'Input or Checkbox'),
  rawElement('select', 'Select'),
  rawElement('textarea', 'Input'),
];
const styleProp = {
  selector: "JSXAttribute[name.name='style']",
  message: guidelines('No `style` prop outside the kit: compose kit parts and theme classes.'),
};
const dynamicBaseUi = {
  selector: "ImportExpression[source.value=/^@base-ui\\/react(\\/|$)/]",
  message: baseUi.message,
};

export default tseslint.config(
  { ignores: ['node_modules/**', 'dist/**', 'storybook-static/**'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    // The lint rules and the catalogue script run on Node.
    files: ['**/*.mjs'],
    languageOptions: { globals: globals.node },
  },
  {
    files: code,
    plugins: { 'react-hooks': reactHooks },
    rules: reactHooks.configs.recommended.rules,
  },
  {
    files: code,
    ignores: exempt,
    plugins: { 'design-tokens': designTokens },
    rules: {
      'design-tokens/no-raw-colour': ['error', { functions: ['rgb', 'rgba', 'hsl', 'hsla', 'hwb', 'lab', 'lch', 'oklab', 'oklch', 'color'] }],
      'design-tokens/no-raw-style-number': ['error', { lengthUnits: ['px', 'rem', 'em'] }],
      'design-tokens/no-arbitrary-class': 'error',
      'design-tokens/ladder-size': 'error',
      'no-restricted-imports': ['error', { patterns: [baseUi] }],
      'no-restricted-syntax': ['error', ...rawElements, styleProp, dynamicBaseUi],
    },
  },
  {
    // The separator rule (guidelines § Copy): copy in the React UI joins facts with
    // one phrase, two lines or a spaced pipe, never a dot.
    files: ['src/**/*.{ts,tsx}'],
    plugins: { 'design-tokens': designTokens },
    rules: { 'design-tokens/no-dot-separator': 'error' },
  },
  {
    // The kit wraps Base UI, renders the raw elements and may set `style` (a positioner's
    // computed offsets, say).
    files: kit,
    rules: {
      'no-restricted-imports': 'off',
      'no-restricted-syntax': 'off',
      'design-tokens/ladder-size': ['error', { allow: ['h', 'w', 'size', 'min-h', 'min-w', 'max-h', 'max-w'] }],
    },
  },
);
