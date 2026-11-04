#!/usr/bin/env node
/**
 * Design catalogue generator: builds one markdown file listing every UI component, pattern block,
 * template and screen, read from their docstrings. Node built-ins only.
 *
 *   node scripts/catalog.mjs                     write the catalogue (path from the config)
 *   node scripts/catalog.mjs --check             exit 1 when the file is stale, a docstring is
 *                                                incomplete or a required story is missing
 *   node scripts/catalog.mjs --config path.json  config path (default: ./design-catalog.config.json)
 *
 * Paths in the config resolve against the config file's directory (the repo root).
 *
 * DOCSTRING FORMAT (a JSDoc block directly above `export function Name` / `export const Name`):
 *
 *   /**
 *    * First paragraph: what it is, one or two sentences.
 *    *
 *    * @useWhen when to pick it, and when not to (name the alternative).
 *    * @states   the states it renders, comma separated (default, pressed, disabled, loading).
 *    * @legacy   optional: present means "not on tokens yet"; the text says what replaces it.
 *    *\/
 *   export function Button(props: ButtonProps) {}
 *
 * Props are documented on the props type/interface, one JSDoc line per prop:
 *
 *   interface ButtonProps {
 *     /** Visible label. *\/
 *     label: string;
 *   }
 *
 * The props type is found by name: `<ComponentName>Props` (suffix configurable). Hooks (`useX`)
 * need neither @states nor a story.
 *
 * CONFIG (design-catalog.config.json): this repository's own is ui/design-catalog.config.json. Group types:
 *   "components"  exported components found in `sources` globs; docstring and story checks.
 *   "routes"      one screen per route file in `sources`; story check only (no docstring).
 *   "legacy"      collects every component tagged @legacy from the other groups.
 * Story convention, `story` template tokens: {dir} source dir, {file} source file name without
 * extension, {name} component name, {key} route key. A story file counts its `export const X`.
 */
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { basename, dirname, extname, join, relative, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const DEFAULT_CONFIG = 'design-catalog.config.json';
const SKIP_DIRS = new Set(['node_modules', 'dist', 'build', 'generated']);
const MAX_TYPE = 110;

// ---------------------------------------------------------------------------------------------
// Config and file discovery

export function loadConfig(configPath = join(process.cwd(), DEFAULT_CONFIG)) {
  const file = resolve(configPath);
  if (!existsSync(file)) throw new Error(`Config not found: ${file}`);
  const config = JSON.parse(readFileSync(file, 'utf8'));
  if (!config.output || !Array.isArray(config.groups)) {
    throw new Error(`${file}: needs "output" and a "groups" array`);
  }
  return { config, root: dirname(file) };
}

/** Glob to RegExp: `**`, `*`, `?`, `{a,b}`. Matches posix-style paths relative to the root. */
export function globToRegExp(glob) {
  let out = '';
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === '*' && glob[i + 1] === '*') {
      if (glob[i + 2] === '/') {
        out += '(?:.*/)?';
        i += 2;
      } else {
        out += '.*';
        i += 1;
      }
    } else if (c === '*') out += '[^/]*';
    else if (c === '?') out += '[^/]';
    else if (c === '{') {
      const end = glob.indexOf('}', i);
      const options = glob.slice(i + 1, end).split(',');
      out += `(?:${options.map((o) => o.replace(/[.+^$()|[\]\\]/g, '\\$&')).join('|')})`;
      i = end;
    } else out += c.replace(/[.+^$()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`^${out}$`);
}

function walk(dir, visit) {
  if (!existsSync(dir)) return;
  for (const item of readdirSync(dir, { withFileTypes: true })) {
    if (item.isDirectory()) {
      if (SKIP_DIRS.has(item.name) || item.name.startsWith('.')) continue;
      walk(join(dir, item.name), visit);
    } else visit(join(dir, item.name));
  }
}

/** Files (relative posix paths, sorted) matching any include glob and no exclude glob. */
export function expandGlobs(root, includes, excludes = []) {
  const inc = includes.map(globToRegExp);
  const exc = excludes.map(globToRegExp);
  const prefixes = new Set(
    includes.map((g) => {
      const parts = g.split('/');
      const stop = parts.findIndex((p) => /[*?{]/.test(p));
      return (stop === -1 ? parts.slice(0, -1) : parts.slice(0, stop)).join('/');
    }),
  );
  const found = new Set();
  for (const prefix of prefixes) {
    walk(join(root, prefix), (abs) => {
      const rel = relative(root, abs).split('\\').join('/');
      if (inc.some((r) => r.test(rel)) && !exc.some((r) => r.test(rel))) found.add(rel);
    });
  }
  return [...found].sort();
}

// ---------------------------------------------------------------------------------------------
// Docstring and props parsing

const clean = (s) => s.replace(/\s*\n\s*/g, ' ').replace(/\s+/g, ' ').trim();

/** Splits a JSDoc body into { what, tags }. Tag text runs to the next tag. */
export function parseDoc(raw) {
  const body = raw
    .replace(/^\/\*\*/, '')
    .replace(/\*\/$/, '')
    .split('\n')
    .map((l) => l.replace(/^\s*\*? ?/, ''))
    .join('\n')
    .trim();
  const [head, ...rest] = body.split(/^@/m);
  const tags = {};
  for (const chunk of rest) {
    const m = /^(\w+)\s*([\s\S]*)$/.exec(chunk);
    if (m) tags[m[1]] = clean(m[2]) || true;
  }
  return { what: clean(head), tags };
}

/** Finds the JSDoc block directly above an export of `name`, or null. */
function docAbove(source, name) {
  const decl = new RegExp(
    `^export\\s+(?:default\\s+)?(?:async\\s+)?(?:function\\s*\\*?\\s*|const\\s+|class\\s+)${name}\\b`,
    'm',
  );
  const m = decl.exec(source);
  if (!m) return null;
  const before = source.slice(0, m.index).trimEnd();
  if (!before.endsWith('*/')) return { raw: '' };
  const start = before.lastIndexOf('/**');
  if (start === -1) return { raw: '' };
  // A plain block comment is not a docstring.
  return { raw: before.slice(start) };
}

/** Names of exported function/const/class declarations (not types, not re-exports). */
function exportedNames(source) {
  const names = [];
  const re = /^export\s+(?:async\s+)?(?:function\s*\*?\s*|const\s+|class\s+)([A-Za-z_$][\w$]*)/gm;
  for (let m = re.exec(source); m; m = re.exec(source)) names.push(m[1]);
  return names;
}

function bodyAfter(source, openIndex) {
  let depth = 0;
  for (let i = openIndex; i < source.length; i++) {
    if (source[i] === '{') depth++;
    else if (source[i] === '}' && --depth === 0) return source.slice(openIndex + 1, i);
  }
  return '';
}

/** Members of `interface <Name>Props` or `type <Name>Props = {`: [{ name, optional, type, doc }]. */
export function parseProps(source, propsName) {
  const re = new RegExp(`(?:interface\\s+${propsName}\\b[^{=]*|type\\s+${propsName}\\s*=\\s*)\\{`);
  const m = re.exec(source);
  if (!m) return [];
  const body = bodyAfter(source, m.index + m[0].length - 1);
  const props = [];
  let pos = 0;
  let doc = '';
  while (pos < body.length) {
    const rest = body.slice(pos);
    const ws = /^\s+/.exec(rest);
    if (ws) {
      pos += ws[0].length;
      continue;
    }
    if (rest.startsWith('/**')) {
      const end = rest.indexOf('*/');
      doc = parseDoc(rest.slice(0, end + 2)).what;
      pos += end + 2;
      continue;
    }
    if (rest.startsWith('//')) {
      pos += (rest.indexOf('\n') + 1 || rest.length);
      continue;
    }
    const member = /^(?:readonly\s+)?(['"]?)([\w$-]+)\1(\?)?\s*:/.exec(rest);
    if (!member) {
      pos += (rest.indexOf('\n') + 1 || rest.length);
      doc = '';
      continue;
    }
    let i = member[0].length;
    let depth = 0;
    for (; i < rest.length; i++) {
      const ch = rest[i];
      if ('({['.includes(ch) || (ch === '<' && rest[i - 1] !== '=')) depth++;
      else if (')}]'.includes(ch) || (ch === '>' && rest[i - 1] !== '=')) depth--;
      else if (depth <= 0 && (ch === ';' || ch === ',')) break;
      else if (depth <= 0 && ch === '\n') {
        const prev = rest.slice(member[0].length, i).trimEnd().slice(-1);
        const next = /^\s*(.)/.exec(rest.slice(i))?.[1];
        if (!'|&:'.includes(prev) && !'|&'.includes(next ?? '')) break;
      }
    }
    let type = clean(rest.slice(member[0].length, i));
    if (type.length > MAX_TYPE) type = `${type.slice(0, MAX_TYPE - 1)}…`;
    props.push({ name: member[2], optional: Boolean(member[3]), type, doc });
    doc = '';
    pos += i + 1;
  }
  return props;
}

// ---------------------------------------------------------------------------------------------
// Stories

/** Number of stories in a story file (its named `export const`s), or 0 when the file is missing. */
export function countStories(file, pattern = '^export const \\w+') {
  if (!existsSync(file)) return 0;
  return (readFileSync(file, 'utf8').match(new RegExp(pattern, 'gm')) ?? []).length;
}

const fill = (template, tokens) => template.replace(/\{(\w+)\}/g, (_, k) => tokens[k] ?? '');

/** Story file path (relative to root) for a source file, component name or route key. */
export function storyPathOf(group, { source, name, key }) {
  const dir = source ? dirname(source) : '';
  const file = source ? basename(source, extname(source)) : '';
  return fill(group.story, { dir, file, name, key }).replace(/^\.\//, '');
}

// ---------------------------------------------------------------------------------------------
// Collection

const isHook = (name) => /^use[A-Z]/.test(name);

/** Entries for every "components" group (and the "legacy" group, filled from the others). */
export function collectEntries({ config, root }) {
  const entries = [];
  const nameRe = new RegExp(config.componentNamePattern ?? '^[A-Z]|^use[A-Z]');
  const suffix = config.propsSuffix ?? 'Props';
  for (const group of config.groups.filter((g) => g.type === 'components')) {
    const files = expandGlobs(root, group.sources, group.exclude ?? []);
    for (const source of files) {
      const text = readFileSync(join(root, source), 'utf8');
      for (const name of exportedNames(text)) {
        if (!nameRe.test(name)) continue;
        if (group.names && !group.names.includes(name)) continue;
        if (group.excludeNames?.includes(name)) continue;
        const doc = parseDoc(docAbove(text, name)?.raw ?? '');
        const hook = isHook(name);
        entries.push({
          name,
          group: group.name,
          what: doc.what,
          useWhen: typeof doc.tags.useWhen === 'string' ? doc.tags.useWhen : '',
          states: typeof doc.tags.states === 'string' ? doc.tags.states : '',
          legacy: typeof doc.tags.legacy === 'string' ? doc.tags.legacy : doc.tags.legacy ? 'yes' : '',
          props: parseProps(text, `${name}${suffix}`),
          source,
          hook,
          requireStory: group.requireStory !== false && !hook,
          requireDocs: group.requireDocs !== false,
          storyFile: storyPathOf(group, { source, name }),
          stories: 0,
        });
      }
    }
  }
  const legacy = config.groups.find((g) => g.type === 'legacy');
  for (const e of entries) {
    if (legacy && e.legacy) e.group = legacy.name;
    e.stories = e.hook ? 0 : countStories(join(root, e.storyFile), config.storyExportPattern);
  }
  const order = config.groups.map((g) => g.name);
  return entries.sort(
    (a, b) => order.indexOf(a.group) - order.indexOf(b.group) || a.name.localeCompare(b.name),
  );
}

/** Screens for every "routes" group: { group, route, key, storyFile }. */
export function collectRoutes({ config, root }) {
  const routes = [];
  for (const group of config.groups.filter((g) => g.type === 'routes')) {
    const base = group.routesDir ?? '.';
    for (const path of expandGlobs(root, group.sources, group.exclude ?? [])) {
      const name = basename(path);
      // Layouts (underscore files) and redirect aliases are not screens.
      if (name.startsWith('_')) continue;
      if (readFileSync(join(root, path), 'utf8').includes(group.aliasTag ?? '@route-alias')) continue;
      const key = relative(base, path)
        .split('\\')
        .join('/')
        // Route groups such as `(tabs)` are not part of a screen's name.
        .replace(/\([^)/]+\)\//g, '')
        .replace(/\.[jt]sx?$/, '')
        .replace(/\/\[[^\]]+\]$/, '')
        .replaceAll('/', '-');
      const storyFile = storyPathOf(group, { key });
      routes.push({
        group: group.name,
        route: path,
        key,
        storyFile,
        requireStory: group.requireStory !== false,
        stories: 0,
      });
    }
  }
  return routes.sort((a, b) => a.route.localeCompare(b.route));
}

/** Docstring gaps that make an entry unusable to whoever composes with it. */
export function problems(entries) {
  const out = [];
  for (const e of entries) {
    if (!e.requireDocs) continue;
    const at = `${e.name} (${e.source})`;
    if (!e.what) out.push(`${at}: no description`);
    if (!e.useWhen) out.push(`${at}: no @useWhen`);
    if (!e.hook && !e.legacy && !e.states) out.push(`${at}: no @states`);
    for (const p of e.props) if (!p.doc) out.push(`${at}: prop "${p.name}" has no doc`);
  }
  return out;
}

/** Every required component and route needs at least one story. `stories` is injectable for tests. */
export function storyProblems(entries, routes, stories = (file) => countStories(file), root = '') {
  const out = [];
  for (const e of entries) {
    if (e.requireStory && stories(join(root, e.storyFile)) === 0) {
      out.push(`${e.name} (${e.source}): no stories in ${e.storyFile}`);
    }
  }
  for (const r of routes) {
    if (r.requireStory && stories(join(root, r.storyFile)) === 0) {
      out.push(`${r.route}: no stories in ${r.storyFile}`);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Rendering

const cell = (s) => s.replace(/\|/g, '\\|').replace(/\n/g, ' ');

export function render(entries, routes, config) {
  const lines = [
    `# ${config.title ?? 'Design catalogue'}`,
    '',
    `<!-- Generated by the catalogue script from component docstrings. Do not edit by hand: run \`${config.command ?? 'npm run catalog'}\`. -->`,
    '',
    ...(config.intro ?? []),
  ];
  const listed = entries.filter((e) => !e.hook);
  lines.push('', '## Stories', '', 'How many stories show each entry. Counted per story file.');
  lines.push('', '| Entry | Group | Stories |', '| --- | --- | --- |');
  for (const e of listed) lines.push(`| ${e.name} | ${e.group} | ${e.stories} |`);
  for (const r of routes) lines.push(`| ${r.route} | ${r.group} | ${r.stories} |`);
  for (const group of config.groups) {
    lines.push('', `## ${group.name}`);
    if (group.intro) lines.push('', group.intro);
    if (group.type === 'routes') {
      const mine = routes.filter((r) => r.group === group.name);
      if (mine.length === 0) lines.push('', '_None yet._');
      else {
        lines.push('', '| Route | Key | Stories |', '| --- | --- | --- |');
        for (const r of mine) lines.push(`| \`${r.route}\` | ${r.key} | ${r.stories} |`);
      }
      continue;
    }
    const mine = entries.filter((e) => e.group === group.name);
    if (mine.length === 0) lines.push('', '_None yet._');
    for (const e of mine) {
      lines.push('', `### ${e.name}`, '', e.what || '_No description._', '');
      lines.push(`- **Use when:** ${e.useWhen || '_missing_'}`);
      if (e.states) lines.push(`- **States:** ${e.states}`);
      if (e.legacy) lines.push(`- **Legacy:** ${e.legacy}`);
      lines.push(`- **Source:** \`${e.source}\``);
      if (!e.hook) lines.push(`- **Stories:** ${e.stories}`);
      if (e.props.length > 0) {
        lines.push('', '| Prop | Type | Description |', '| --- | --- | --- |');
        for (const p of e.props) {
          lines.push(`| \`${p.optional ? `${p.name}?` : p.name}\` | \`${cell(p.type)}\` | ${cell(p.doc)} |`);
        }
      }
    }
  }
  return `${lines.join('\n')}\n`;
}

// ---------------------------------------------------------------------------------------------
// CLI

function main() {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const at = args.indexOf('--config');
  const { config, root } = loadConfig(at === -1 ? undefined : args[at + 1]);
  const entries = collectEntries({ config, root });
  const routes = collectRoutes({ config, root });
  for (const r of routes) r.stories = countStories(join(root, r.storyFile), config.storyExportPattern);

  const found = [
    ...problems(entries),
    ...storyProblems(entries, routes, (f) => countStories(f, config.storyExportPattern), root),
  ];
  if (found.length > 0) {
    console.error(`Catalogue problems:\n${found.map((p) => `  ${p}`).join('\n')}`);
    process.exit(1);
  }
  const out = join(root, config.output);
  const next = render(entries, routes, config);
  if (check) {
    const current = existsSync(out) ? readFileSync(out, 'utf8') : '';
    if (current !== next) {
      console.error(`${config.output} is stale. Run \`${config.command ?? 'npm run catalog'}\` and commit the result.`);
      process.exit(1);
    }
    console.log(`${config.output} is up to date (${entries.length} entries, ${routes.length} routes).`);
    return;
  }
  writeFileSync(out, next);
  console.log(`Wrote ${config.output} (${entries.length} entries, ${routes.length} routes).`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
