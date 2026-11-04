import { call, headers } from '../session';
import type { Answered, Config, RawAnswer, RawFile, SaveResult, SettingsView } from './types';

// The session plumbing lives in ../session; re-exported so the settings screen keeps one import.
export { isSessionEnded, SessionEnded, sessionEndedWhy } from '../session';

/** GET /api/settings: the config, its defaults and the schema to render it with. */
export async function load(): Promise<SettingsView> {
  const res = await call('/api/settings', { headers: headers() });
  if (!res.ok) throw new Error(`settings answered ${res.status}`);
  return (await res.json()) as SettingsView;
}

/** The body as JSON, or a failed SaveResult carrying the plain text a non-JSON answer sent. */
async function body(res: Response): Promise<Record<string, unknown>> {
  const text = await res.text();
  try {
    return JSON.parse(text) as Record<string, unknown>;
  } catch {
    return { saved: false, errors: [text || `faber answered ${res.status}`] };
  }
}

/**
 * POST /api/settings?base=<digest>. Returns the parsed answer and its `status` whatever the code:
 * 200 saved, 400 `errors`, 403 `refused`, 409 stale base, 500 write failure. The caller reads `saved`.
 */
export async function save(config: Config, base?: string): Promise<Answered<SaveResult>> {
  const query = base ? `?base=${encodeURIComponent(base)}` : '';
  const res = await call(`/api/settings${query}`, {
    method: 'POST',
    headers: headers({ 'Content-Type': 'application/json' }),
    body: JSON.stringify(config),
  });
  return { saved: false, errors: [], ...(await body(res)), status: res.status } as unknown as Answered<SaveResult>;
}

/** GET /api/settings/raw: config.toml as text, with its path and the digest to save against. */
export async function loadRaw(): Promise<RawFile> {
  const res = await call('/api/settings/raw', { headers: headers() });
  if (!res.ok) throw new Error(`settings answered ${res.status}`);
  return (await res.json()) as RawFile;
}

/**
 * POST /api/settings/raw?base=<digest>[&dry_run=1] with `{text}`. A dry run runs the form's checks
 * and writes nothing. Answers a `SaveResult` with its status, or `{tomlError}` for a 400 that
 * names a line (`{line, column, message}`).
 */
export async function saveRaw(text: string, base: string | undefined, dryRun = false): Promise<RawAnswer> {
  const params = new URLSearchParams();
  if (base) params.set('base', base);
  if (dryRun) params.set('dry_run', '1');
  const query = params.toString() ? `?${params.toString()}` : '';
  const res = await call(`/api/settings/raw${query}`, {
    method: 'POST',
    headers: headers({ 'Content-Type': 'application/json' }),
    body: JSON.stringify({ text }),
  });
  const json = await body(res);
  if (res.status === 400 && typeof json.line === 'number' && typeof json.message === 'string') {
    return { status: 400, tomlError: { line: json.line, column: typeof json.column === 'number' ? json.column : 1, message: json.message } };
  }
  return { saved: false, errors: [], ...json, status: res.status } as unknown as Answered<SaveResult>;
}
