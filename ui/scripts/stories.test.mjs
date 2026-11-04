// Renders every story to markup through Vite's SSR loader, as kit.test.mjs renders the kit, so a
// story that throws fails `npm test` rather than the owner's story browser. It runs each story's
// render with its merged args, without decorators or play functions: popups that a play function
// opens are not checked here, only that the closed state renders. That every kit part has a
// story file at all is `npm run catalog:check`'s job.
import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
// Every directory that holds stories: the kit, the popover's blocks, and the screens.
const dirs = ['kit', 'blocks', 'screens'];
const files = dirs
  .flatMap((dir) => readdirSync(join(ui, 'src', dir)).filter((f) => f.endsWith('.stories.tsx')).map((f) => `${dir}/${f}`))
  .sort();
let server;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
});
after(() => server?.close());

for (const file of files) {
  test(`every story in ${file} renders`, async () => {
    const { default: meta, ...stories } = await server.ssrLoadModule(`/src/${file}`);
    assert.ok(meta?.title, `${file} has no default export with a title`);
    const { TooltipProvider } = await server.ssrLoadModule('/src/kit/index.ts');
    assert.ok(Object.keys(stories).length > 0, `${file} exports no story`);
    for (const [name, story] of Object.entries(stories)) {
      const args = { ...meta.args, ...story.args };
      const render = story.render ?? meta.render ?? ((a) => React.createElement(meta.component, a));
      const html = renderToStaticMarkup(React.createElement(TooltipProvider, null, render(args, { args })));
      assert.ok(html.length > 0, `${file} ${name} rendered nothing`);
    }
  });
}
