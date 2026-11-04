import type { Config, FieldSpec, FormSchema } from './types';

// The draft model: a copy of the config the form edits, read back field by field. A port of
// desktop/src/settings.js, which it replaces; the blank, optional and unstated rules are kept exactly.

/** The value at a dotted key, or undefined when any step is missing. */
export function dig(obj: unknown, path: string): unknown {
  let o = obj;
  for (const k of path.split('.')) {
    if (o == null || typeof o !== 'object') return undefined;
    o = (o as Record<string, unknown>)[k];
  }
  return o;
}

/** Sets the value at a dotted key, making the objects on the way. */
export function plant(obj: Config, path: string, val: unknown): void {
  const keys = path.split('.');
  const last = keys.pop() as string;
  let o: Record<string, unknown> = obj;
  for (const k of keys) {
    const next = o[k];
    if (next == null || typeof next !== 'object') o[k] = {};
    o = o[k] as Record<string, unknown>;
  }
  o[last] = val;
}

/** A field's value in the text its input shows. */
export function asText(field: FieldSpec, v: unknown): string {
  if (v == null) return '';
  if (field.kind === 'csv') return (v as unknown[]).join(', ');
  if (field.kind === 'lines') return (v as unknown[]).join('\n');
  if (field.kind === 'map') return Object.entries(v as Record<string, unknown>).map(([k, x]) => `${k}=${x}`).join('\n');
  return String(v);
}

/**
 * Reads one input back in the shape the config expects. `text` is what the input holds: a string,
 * a boolean for a required bool, and for a `ceiling` either `auto`, `off` or the number as text.
 * An empty optional field is null (unset); an empty required one falls back to faber's default.
 * A `sudo` or `held` field is never parsed back: it round-trips as loaded.
 */
export function readValue(field: FieldSpec, text: string | boolean, loaded: unknown, defaults?: Config): unknown {
  if (field.sudo || field.held) return loaded;
  const t = typeof text === 'string' ? text.trim() : text;
  switch (field.kind) {
    case 'bool':
      if (!field.optional) return t === true || t === 'true';
      if (t === '') return field.unstated_saves != null ? field.unstated_saves !== 0 : null;
      return t === 'true';
    case 'enum':
      return text;
    case 'ceiling': {
      if (text === 'auto' || text === 'off') return text;
      const n = Number(text);
      return Number.isFinite(n) && n > 0 ? Math.round(n) : 'off';
    }
    case 'csv':
      return String(t).split(',').map((s) => s.trim()).filter(Boolean);
    case 'lines':
      return String(t).split('\n').map((s) => s.trim()).filter(Boolean);
    case 'map': {
      const out: Record<string, string> = {};
      for (const line of String(t).split('\n')) {
        const l = line.trim();
        if (!l) continue;
        const i = l.indexOf('=');
        if (i < 0) {
          out[l] = '';
          continue;
        }
        out[l.slice(0, i).trim()] = l.slice(i + 1).trim();
      }
      return out;
    }
    case 'int':
    case 'float': {
      const blank = field.unstated_saves != null ? field.unstated_saves : field.optional ? null : (dig(defaults, field.key) ?? null);
      if (t === '') return blank;
      const n = Number(t);
      if (!Number.isFinite(n)) return blank;
      return field.kind === 'int' ? Math.round(n) : n;
    }
    default:
      return t === '' && field.optional ? null : t;
  }
}

/** One changed key and when it takes effect. */
export interface Change {
  /** The dotted key; a profile field is `profile.<index>.<key>`, a added or removed profile `profile`. */
  key: string;
  label: string;
  effect: 'live' | 'restart';
}

const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/** The keys the draft changes against the loaded config, each tagged `live` or `restart`. */
export function changes(draft: Config, loaded: Config, schema: FormSchema): Change[] {
  const out: Change[] = [];
  for (const section of schema.sections) {
    for (const f of section.fields) {
      if (!same(dig(draft, f.key), dig(loaded, f.key))) out.push({ key: f.key, label: f.label, effect: f.live ? 'live' : 'restart' });
    }
  }
  const now = (draft.profile as Config[] | undefined) ?? [];
  const was = (loaded.profile as Config[] | undefined) ?? [];
  if (now.length !== was.length) out.push({ key: 'profile', label: 'Profiles', effect: 'restart' });
  now.forEach((p, i) => {
    if (i >= was.length) return;
    for (const f of schema.profile_fields) {
      if (!same(dig(p, f.key), dig(was[i], f.key))) out.push({ key: `profile.${i}.${f.key}`, label: f.label, effect: f.live ? 'live' : 'restart' });
    }
  });
  return out;
}

const SAFE_WORD = /^[A-Za-z0-9._,/:@=+-]+$/;

/** One shell word, exactly as `control::shell_quote` writes it (empty is quoted). */
export const shq = (v: string): string => (SAFE_WORD.test(v) ? v : `'${v.replace(/'/g, `'\\''`)}'`);

/** The sudo template with the profile's name in: the word holding `<profile>` is filled and quoted whole. */
export function sudoCmd(template: string, profile?: { name: string } | null): string {
  if (!profile) return template;
  return template
    .split(' ')
    .map((w) => (w.indexOf('<profile>') < 0 ? w : shq(w.split('<profile>').join(profile.name))))
    .join(' ');
}

/** Would the narrowing change the value, and only towards narrower? Numbers: only when below it; else when it differs (null-aware). */
export function narrowShown(field: FieldSpec, v: unknown): boolean {
  if (!field.narrow) return false;
  const n = JSON.parse(field.narrow.value) as unknown;
  if (typeof n === 'number') return typeof v === 'number' && v < n;
  const cur = v == null ? (Array.isArray(n) ? [] : null) : v;
  return JSON.stringify(cur) !== JSON.stringify(n);
}
