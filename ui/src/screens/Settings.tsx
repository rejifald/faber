import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ConfirmRow } from '../blocks/ConfirmRow';
import { EmptyProfiles } from '../blocks/EmptyProfiles';
import { FieldRow } from '../blocks/FieldRow';
import { Notice } from '../blocks/Notice';
import { ProfileRow, profileFacts } from '../blocks/ProfileRow';
import { RawEditor } from '../blocks/RawEditor';
import { SaveBar, type SaveBarChange } from '../blocks/SaveBar';
import { SectionHeader } from '../blocks/SectionHeader';
import { SettingsNav, type NavResult } from '../blocks/SettingsNav';
import { ViewHeader } from '../blocks/ViewHeader';
import { Button } from '../kit';
import * as realApi from '../settings/api';
import { changes as diff, dig, plant, type Change } from '../settings/draft';
import { changedLines } from '../settings/raw';
import { matches } from '../settings/search';
import { navItems, PROFILES_ID, RAW_ID, viewSections } from '../settings/nav';
import type { Answered, Config, FieldSpec, RawAnswer, RawFile, Refused, SaveResult, SettingsView, TomlError } from '../settings/types';

/** What the view needs from the daemon; stories pass fakes, the page passes `settings/api`. */
export interface SettingsApi {
  load: () => Promise<SettingsView>;
  save: (config: Config, base?: string) => Promise<Answered<SaveResult>>;
  loadRaw: () => Promise<RawFile>;
  saveRaw: (text: string, base: string | undefined, dryRun?: boolean) => Promise<RawAnswer>;
}

/** How the last save ended, when it did not save. */
export type Outcome = { kind: 'invalid'; errors: string[] } | { kind: 'refused'; refused: Refused[] } | { kind: 'stale' } | { kind: 'failed'; message: string };

/** The raw editor's own state. */
export interface RawState {
  file?: RawFile;
  /** The text as edited. */
  text: string;
  loading: boolean;
  loadFailed?: string;
  /** A TOML error, from the last dry run or save. */
  error?: TomlError;
  /** The dry run's or save's other outcome: errors, refused or stale. */
  outcome?: Outcome;
  /** What the parsed text changes, from the last good dry run. */
  preview?: Change[];
  saving: boolean;
}

/** Everything the view keeps. `initial` seeds it, so a story can show any state without a daemon. */
export interface SettingsState {
  phase: 'loading' | 'ready' | 'error';
  loadError?: string;
  view?: SettingsView;
  draft: Config;
  base?: string;
  saving: boolean;
  outcome?: Outcome;
  /** Messages per field key (`profile.<index>.<key>` in a profile), from the daemon or the form's own checks. */
  fieldErrors: Record<string, string>;
  /** The last save succeeded in this session, so the restart notice says "Saved". */
  justSaved: boolean;
  section?: string;
  query: string;
  profile: number | null;
  confirmRemove: boolean;
  raw: RawState;
  /** Why the session ended, once a call answered 401: the view then says so and calls nothing more. */
  sessionEnded?: string;
}

export interface SettingsProps {
  /** The daemon calls; the page passes `settings/api`, stories and tests pass fakes. */
  api?: SettingsApi;
  /** Seeds the state. With a `view` the page does not load first. */
  initial?: Partial<SettingsState>;
}

/**
 * `api`, stopped by a 401: `ended` hears why, once, and that call and every later one never settle,
 * so no handler acts on an answer the view no longer shows. No retry: a reload cannot bring a key back.
 */
function guarded(api: SettingsApi, ended: (why: string) => void): SettingsApi {
  let gone = false;
  const wrap =
    <A extends unknown[], R>(f: (...a: A) => Promise<R>) =>
    async (...a: A): Promise<R> => {
      if (gone) return new Promise<R>(() => {});
      try {
        return await f(...a);
      } catch (e) {
        if (!realApi.isSessionEnded(e)) throw e;
        if (!gone) ended(e.why);
        gone = true;
        return new Promise<R>(() => {});
      }
    };
  return { load: wrap(api.load), save: wrap(api.save), loadRaw: wrap(api.loadRaw), saveRaw: wrap(api.saveRaw) };
}

const emptyRaw: RawState = { text: '', loading: false, saving: false };
const clone = <T,>(v: T): T => structuredClone(v);
const profilesOf = (config: Config): Config[] => (Array.isArray(config.profile) ? (config.profile as Config[]) : []);
const effectLive = (c: Change): SaveBarChange => ({ key: c.key, label: c.label, live: c.effect === 'live' });

/** A new profile: last in the order, `faber` as its label, Docker access stated off (a blank grants it). */
export const newProfile = (): Config => ({ name: '', labels: ['faber'], backend: 'docker', dind: false });

/** The profile fields' groups, in the order they show (anatomy.md § 5, Settings > Profiles). */
const PROFILE_GROUPS = ['Basics', 'Trust', 'Speed', 'Virtual machine'] as const;
type ProfileGroup = (typeof PROFILE_GROUPS)[number];
function profileGroup(f: FieldSpec): ProfileGroup {
  if (f.sudo) return 'Trust';
  if (f.key === 'git_mirror' || f.key === 'mirror') return 'Speed';
  if (f.key === 'cpus' || f.key === 'memory_mib') return 'Virtual machine';
  return 'Basics';
}

/** A save body: the draft, with every blank `unstated_saves` field written out as its stated value. */
export function prepared(draft: Config, view: SettingsView): Config {
  const out = clone(draft);
  const fix = (obj: Config, f: FieldSpec) => {
    if (f.unstated_saves == null || dig(obj, f.key) != null) return;
    plant(obj, f.key, f.kind === 'bool' ? f.unstated_saves !== 0 : f.unstated_saves);
  };
  for (const s of view.schema.sections) for (const f of s.fields) fix(out, f);
  for (const p of profilesOf(out)) for (const f of view.schema.profile_fields) fix(p, f);
  return out;
}

/** The form's own checks before a post: a profile needs a name nobody else has. Keys are `profile.<i>.name`. */
export function localErrors(draft: Config): Record<string, string> {
  const out: Record<string, string> = {};
  const names = profilesOf(draft).map((p) => String(p.name ?? '').trim());
  names.forEach((name, i) => {
    if (name === '') out[`profile.${i}.name`] = 'Name the profile.';
    else if (names.indexOf(name) !== i) out[`profile.${i}.name`] = `A profile called ${name} already exists.`;
  });
  return out;
}

/** Attaches each daemon error to the fields whose label or key it names. The rest stay unmatched. */
export function fieldErrorsFrom(errors: string[], view: SettingsView, draft: Config): { marked: Record<string, string>; unmatched: string[] } {
  const marked: Record<string, string> = {};
  const unmatched: string[] = [];
  const mentions = (e: string, f: FieldSpec) => e.toLowerCase().includes(f.label.toLowerCase()) || e.includes(f.key);
  for (const e of errors) {
    let hit = false;
    for (const s of view.schema.sections) {
      for (const f of s.fields) {
        if (mentions(e, f)) {
          marked[f.key] ??= e;
          hit = true;
        }
      }
    }
    profilesOf(draft).forEach((p, i) => {
      const named = e.includes(String(p.name ?? '')) && String(p.name ?? '') !== '';
      if (!named && !e.includes(`profile[${i}]`)) return;
      for (const f of view.schema.profile_fields) {
        if (mentions(e, f)) {
          marked[`profile.${i}.${f.key}`] ??= e;
          hit = true;
        }
      }
    });
    if (!hit) unmatched.push(e);
  }
  return { marked, unmatched };
}

/** The raw save bar's sentence: "2 lines changed. Headroom applies now, sweep interval on restart." */
export function rawSummary(lines: number, preview: Change[] | undefined): string {
  const head = `${lines} ${lines === 1 ? 'line' : 'lines'} changed.`;
  if (!preview || preview.length === 0) return head;
  const named = preview.slice(0, 2);
  const more = preview.length - named.length;
  const parts: string[] = [];
  for (const [effect, text] of [['live', 'applies now'], ['restart', 'on restart']] as const) {
    const group = named.filter((c) => c.effect === effect);
    if (group.length === 0) continue;
    const labels = group.map((c, i) => (i === 0 && parts.length === 0 ? c.label : c.label.toLowerCase())).join(', ');
    parts.push(effect === 'live' ? `${labels} ${group.length === 1 ? 'applies' : 'apply'} now` : `${labels} ${text}`);
  }
  return `${head} ${parts.join(', ')}${more > 0 ? ` and ${more} more` : ''}.`;
}

function CopyCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      onClick={() => {
        void navigator.clipboard?.writeText(command).then(
          () => setCopied(true),
          () => {},
        );
      }}
    >
      {copied ? 'Copied' : 'Copy command'}
    </Button>
  );
}

function RefusedNotices({ refused, onDiscard }: { refused: Refused[]; onDiscard: () => void }) {
  return (
    <>
      {refused.map((r) => (
        <Notice
          key={r.key}
          tone="red"
          title="Not saved: this widens trust"
          body={`${r.sentence} A change like this needs ${r.needs}.`}
          command={r.command}
          actions={
            <>
              <Button variant="link" onClick={onDiscard}>Discard</Button>
              <CopyCommand command={r.command} />
            </>
          }
        />
      ))}
    </>
  );
}

function Skeleton() {
  return (
    <div aria-busy="true" aria-label="Loading settings" className="flex flex-col gap-lg">
      <div className="flex flex-col gap-xs">
        <div className="h-3xl w-label rounded-sm bg-surface-2" />
        <div className="h-xl w-label rounded-sm bg-surface-2" />
      </div>
      {[0, 1, 2, 3].map((i) => (
        <div key={i} className="flex items-center gap-lg border-b border-border-soft py-lg">
          <div className="h-3xl min-w-0 flex-1 rounded-sm bg-surface-2" />
          <div className="h-3xl w-field rounded-sm bg-surface-2" />
        </div>
      ))}
    </div>
  );
}

const hashOf = () => (typeof location === 'undefined' ? '' : location.hash.replace(/^#/, ''));

/**
 * The Settings page (anatomy.md § 5, Settings): the nav, the selected section rendered from the
 * schema, the raw `config.toml` editor, and the save bar. One draft at a time between form and text.
 *
 * @useWhen the dashboard's /settings page. Its state is seeded by `initial` for stories and tests.
 * @states loading, loaded, saving, saved with restart pending, invalid, refused, stale, can't load, started with flags, no profiles, raw clean, raw changed, raw TOML error, raw refused, raw blocked by form changes, raw stale, session ended
 */
export function Settings({ api: given = realApi, initial }: SettingsProps) {
  const [st, setSt] = useState<SettingsState>(() => ({
    phase: initial?.view ? 'ready' : 'loading',
    draft: initial?.view ? clone(initial.view.config) : {},
    base: initial?.view?.base,
    saving: false,
    fieldErrors: {},
    justSaved: false,
    query: '',
    profile: null,
    confirmRemove: false,
    raw: emptyRaw,
    ...initial,
  }));
  const api = useMemo(() => guarded(given, (why) => setSt((s) => ({ ...s, sessionEnded: why }))), [given]);
  const patch = (p: Partial<SettingsState>) => setSt((s) => ({ ...s, ...p }));
  const patchRaw = (p: Partial<RawState>) => setSt((s) => ({ ...s, raw: { ...s.raw, ...p } }));
  const { view, draft } = st;

  // The section lives in the URL hash, so a reload and the back button keep it.
  useEffect(() => {
    const onHash = () => setSt((s) => ({ ...s, section: hashOf() || undefined, profile: null, confirmRemove: false }));
    if (hashOf()) onHash();
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);

  const refresh = async () => {
    try {
      const next = await api.load();
      setSt((s) => ({ ...s, phase: 'ready', view: next, draft: clone(next.config), base: next.base, loadError: undefined }));
    } catch (e) {
      setSt((s) => ({ ...s, phase: 'error', loadError: e instanceof Error ? e.message : String(e) }));
    }
  };
  const started = useRef(false);
  useEffect(() => {
    if (started.current || initial?.view) return;
    started.current = true;
    void refresh();
    // Once, on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const sections = view ? viewSections(view.schema) : [];
  const profiles = profilesOf(draft);
  const items = view ? navItems(view.schema, profiles.length, true) : [];
  const selected = st.section && items.some((i) => i.id === st.section) ? st.section : items[0]?.id;

  const loadedProfiles = view ? profilesOf(view.config) : [];
  const formChanges: Change[] = view ? diff(draft, view.config, view.schema) : [];
  const formDirty = formChanges.length > 0;
  const rawDirty = st.raw.file !== undefined && st.raw.text !== st.raw.file.text;

  const select = (id: string) => {
    if (typeof location !== 'undefined') location.hash = id;
    patch({ section: id, profile: null, confirmRemove: false, query: '' });
    if (id === RAW_ID && !st.raw.file && !st.raw.loading) void openRaw();
  };

  const openRaw = async () => {
    patchRaw({ loading: true, loadFailed: undefined });
    try {
      const file = await api.loadRaw();
      patchRaw({ file, text: file.text, loading: false, error: undefined, outcome: undefined, preview: undefined });
    } catch (e) {
      patchRaw({ loading: false, loadFailed: e instanceof Error ? e.message : String(e) });
    }
  };
  // A direct visit to #config-toml opens the file without a click.
  const rawOpened = useRef(false);
  useEffect(() => {
    if (selected === RAW_ID && view && !st.raw.file && !rawOpened.current) {
      rawOpened.current = true;
      void openRaw();
    }
    // openRaw reads api and patchRaw only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, view]);

  // Typing pauses 600 ms, then a dry run reports the live/restart effect and any error.
  const dryRun = useRef(0);
  useEffect(() => {
    const file = st.raw.file;
    if (!file || !view || selected !== RAW_ID || st.raw.text === file.text) return;
    const run = ++dryRun.current;
    const timer = setTimeout(() => {
      void api.saveRaw(st.raw.text, file.base, true).then((res) => {
        if (run !== dryRun.current) return;
        if ('tomlError' in res) return patchRaw({ error: res.tomlError, outcome: undefined, preview: undefined });
        if (res.status >= 200 && res.status < 300) {
          return patchRaw({ error: undefined, outcome: undefined, preview: diff(res.preview ?? view.config, view.config, view.schema) });
        }
        patchRaw({ error: undefined, outcome: outcomeOf(res), preview: undefined });
      }, () => {});
    }, 600);
    return () => clearTimeout(timer);
    // The dry run follows the text; the rest it reads is stable between edits.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [st.raw.text, st.raw.file?.text, selected]);

  const outcomeOf = (res: Answered<SaveResult>): Outcome => {
    if (res.status === 409 && res.from_flags) return { kind: 'failed', message: res.errors?.[0] ?? 'faber was started from flags, so there is no file to write.' };
    if (res.status === 409) return { kind: 'stale' };
    if (res.status === 403 && res.refused?.length) return { kind: 'refused', refused: res.refused };
    if (res.errors?.length) return { kind: 'invalid', errors: res.errors };
    return { kind: 'failed', message: `faber answered ${res.status}.` };
  };

  const edit = (key: string, value: unknown) => {
    const next = clone(draft);
    plant(next, key, value);
    const { [key]: _gone, ...rest } = st.fieldErrors;
    void _gone;
    patch({ draft: next, fieldErrors: rest, outcome: st.outcome?.kind === 'refused' ? undefined : st.outcome });
  };
  const editProfile = (index: number, key: string, value: unknown) => {
    const next = clone(draft);
    plant(profilesOf(next)[index], key, value);
    const { [`profile.${index}.${key}`]: _gone, ...rest } = st.fieldErrors;
    void _gone;
    patch({ draft: next, fieldErrors: rest });
  };
  const discard = () => {
    if (!view) return;
    patch({ draft: clone(view.config), outcome: undefined, fieldErrors: {}, profile: null, confirmRemove: false });
  };
  const discardRaw = () => patchRaw({ text: st.raw.file?.text ?? '', error: undefined, outcome: undefined, preview: undefined });

  const settle = (res: Answered<SaveResult>) => {
    patch({
      saving: false,
      view: res.settings,
      draft: clone(res.settings.config),
      base: res.base ?? res.settings.base,
      outcome: undefined,
      fieldErrors: {},
      justSaved: true,
      confirmRemove: false,
      profile: st.profile !== null && st.profile < profilesOf(res.settings.config).length ? st.profile : null,
    });
  };

  const save = async () => {
    if (!view) return;
    const local = localErrors(draft);
    if (Object.keys(local).length > 0) {
      const first = Number(Object.keys(local)[0].split('.')[1]);
      patch({ fieldErrors: local, outcome: undefined, section: PROFILES_ID, profile: first });
      return;
    }
    patch({ saving: true, outcome: undefined, fieldErrors: {} });
    let res: Answered<SaveResult>;
    try {
      res = await api.save(prepared(draft, view), st.base);
    } catch (e) {
      return patch({ saving: false, outcome: { kind: 'failed', message: e instanceof Error ? e.message : String(e) } });
    }
    if (res.status >= 200 && res.status < 300 && res.saved !== false) return settle(res);
    if (res.status === 409) {
      // The file changed elsewhere: take the new one and keep nothing of the draft (no merge).
      patch({ saving: false, outcome: { kind: 'stale' }, fieldErrors: {} });
      return void (await refresh());
    }
    const out = outcomeOf(res);
    if (out.kind === 'invalid') {
      const { marked } = fieldErrorsFrom(out.errors, view, draft);
      return patch({ saving: false, outcome: out, fieldErrors: marked });
    }
    patch({ saving: false, outcome: out });
  };

  const saveRaw = async () => {
    const file = st.raw.file;
    if (!file || !view) return;
    patchRaw({ saving: true });
    let res: RawAnswer;
    try {
      res = await api.saveRaw(st.raw.text, file.base, false);
    } catch (e) {
      return patchRaw({ saving: false, outcome: { kind: 'failed', message: e instanceof Error ? e.message : String(e) } });
    }
    if ('tomlError' in res) return patchRaw({ saving: false, error: res.tomlError, outcome: undefined });
    if (res.status >= 200 && res.status < 300 && res.saved !== false) {
      patch({ view: res.settings, draft: clone(res.settings.config), base: res.base ?? res.settings.base, justSaved: true });
      return patchRaw({ saving: false, file: { ...file, text: st.raw.text, base: res.base ?? res.settings.base ?? file.base }, error: undefined, outcome: undefined, preview: undefined });
    }
    patchRaw({ saving: false, error: undefined, outcome: outcomeOf(res) });
  };

  const reloadAll = async () => {
    patchRaw({ file: undefined, text: '', outcome: undefined, error: undefined, preview: undefined });
    rawOpened.current = false;
    await refresh();
    await openRaw();
  };

  const results: NavResult[] = view
    ? matches(st.query, { sections, profile_fields: [] }).map((h) => ({ id: sections.find((x) => x.title === h.section.title)?.id ?? '', section: h.section.title, field: h.field.label }))
    : [];

  // ---- content

  const unmatched = st.outcome?.kind === 'invalid' && view ? fieldErrorsFrom(st.outcome.errors, view, draft).unmatched : [];
  const notices: ReactNode[] = [];
  if (view?.from_flags && selected !== RAW_ID) {
    notices.push(
      <Notice key="flags" tone="amber" title="Started with flags" body="faber started without a config file, so it claims no jobs. Add a profile and save to write one; faber uses it after a restart." />,
    );
  }
  if (view && view.pending_restart.length > 0) {
    const label = (key: string) => {
      for (const s of view.schema.sections) for (const f of s.fields) if (f.key === key) return f.label;
      for (const f of view.schema.profile_fields) if (key.endsWith(`.${f.key}`) || key === f.key) return f.label;
      return key;
    };
    notices.push(
      <Notice
        key="restart"
        tone="amber"
        title={st.justSaved ? 'Saved. Restart to apply' : 'Restart to apply'}
        body={`${[...new Set(view.pending_restart.map(label))].join(', ')}. Running jobs finish first.`}
      />,
    );
  }
  const out = st.outcome;
  if (out?.kind === 'invalid' && unmatched.length > 0) {
    notices.push(<Notice key="invalid" tone="red" title="Not saved" body={unmatched.join(' ')} />);
  }
  if (out?.kind === 'invalid' && unmatched.length === 0 && Object.keys(st.fieldErrors).length === 0) {
    notices.push(<Notice key="invalid" tone="red" title="Not saved" body={out.errors.join(' ')} />);
  }
  if (out?.kind === 'refused') notices.push(<RefusedNotices key="refused" refused={out.refused} onDiscard={discard} />);
  if (out?.kind === 'stale') {
    notices.push(
      <Notice
        key="stale"
        tone="amber"
        title="Changed on disk meanwhile"
        body="config.toml changed since you opened it, so it was not saved. The form shows the new file; your edits were dropped."
      />,
    );
  }
  if (out?.kind === 'failed') notices.push(<Notice key="failed" tone="red" title="Not saved" body={out.message} />);

  const sectionBody = (id: string): ReactNode => {
    const s = sections.find((x) => x.id === id);
    if (!view || !s) return null;
    return (
      <>
        <SectionHeader title={s.title} hint={s.hint} />
        <div className="flex flex-col">
          {s.fields.map((f) => (
            <FieldRow
              key={f.key}
              field={f}
              value={dig(draft, f.key)}
              loaded={dig(view.config, f.key)}
              defaults={view.defaults}
              tagged={!f.sudo && !f.held}
              error={st.fieldErrors[f.key]}
              onChange={(v) => edit(f.key, v)}
            />
          ))}
        </div>
      </>
    );
  };

  const profileList = (): ReactNode => {
    if (!view) return null;
    const add = () => {
      const next = clone(draft);
      next.profile = [...profilesOf(next), newProfile()];
      patch({ draft: next, profile: profilesOf(next).length - 1, confirmRemove: false });
    };
    return (
      <>
        <SectionHeader
          title="Profiles"
          hint="What a job runs in. A job goes to the first profile whose labels its runs-on matches, so the order matters. Applies on restart."
          action={
            profiles.length > 0 ? (
              <Button onClick={add}>Add profile</Button>
            ) : undefined
          }
        />
        {profiles.length === 0 ? (
          <EmptyProfiles onAdd={add} dirty={formDirty && !rawDirty} />
        ) : (
          <div className="flex flex-col">
            {profiles.map((p, i) => (
              <ProfileRow
                key={i}
                order={i + 1}
                name={String(p.name ?? '') || 'New profile'}
                labels={Array.isArray(p.labels) ? p.labels.map(String) : []}
                facts={profileFacts(p)}
                onOpen={() => patch({ profile: i, confirmRemove: false })}
              />
            ))}
            <p className="px-sm py-md text-small text-muted">
              No drag to reorder: a move that shifts jobs onto a profile with Docker access widens trust and needs <code className="font-mono">sudo faber config apply</code>.
            </p>
          </div>
        )}
      </>
    );
  };

  const profileView = (index: number): ReactNode => {
    if (!view) return null;
    const p = profiles[index];
    if (!p) return null;
    const added = index >= loadedProfiles.length;
    const loaded = loadedProfiles[index] ?? p;
    const name = String(p.name ?? '');
    const remove = () => {
      const next = clone(draft);
      next.profile = profilesOf(next).filter((_, i) => i !== index);
      patch({ draft: next, profile: null, confirmRemove: false, fieldErrors: {} });
    };
    return (
      <>
        <ViewHeader
          title={name || 'New profile'}
          crumb={{ label: 'Profiles', onClick: () => patch({ profile: null, confirmRemove: false }) }}
          action={
            added ? (
              <Button variant="link" onClick={remove}>Cancel</Button>
            ) : (
              <Button tone="danger" onClick={() => patch({ confirmRemove: true })}>
                Remove
              </Button>
            )
          }
        />
        {st.confirmRemove && !added && (
          <ConfirmRow
            question={`Remove ${name}?`}
            consequence={`Jobs for ${Array.isArray(p.labels) ? p.labels.join(', ') : ''} go to the next profile that matches, or wait if none does.`}
            onCancel={() => patch({ confirmRemove: false })}
            onConfirm={remove}
          />
        )}
        {PROFILE_GROUPS.map((group) => {
          const fields = view.schema.profile_fields.filter((f) => profileGroup(f) === group && (group !== 'Virtual machine' || p.backend === 'macos-vm'));
          if (fields.length === 0) return null;
          return (
            <section key={group} aria-label={group} className="flex flex-col">
              <h3 className="pt-lg text-small font-semibold text-muted">{group}</h3>
              {fields.map((f) => {
                const wide = f.kind === 'csv' || f.kind === 'lines' || f.kind === 'map' || (f.sudo !== undefined && !f.narrows);
                return (
                  <FieldRow
                    key={f.key}
                    field={f}
                    value={dig(p, f.key)}
                    loaded={dig(loaded, f.key)}
                    defaults={view.defaults}
                    profile={{ name }}
                    tagged={false}
                    layout={wide ? 'stacked' : 'inline'}
                    error={st.fieldErrors[`profile.${index}.${f.key}`]}
                    onChange={(v) => editProfile(index, f.key, v)}
                  />
                );
              })}
            </section>
          );
        })}
        {added && <p className="py-md text-small text-muted">Added last, so it takes only jobs no earlier profile matches.</p>}
      </>
    );
  };

  const rawBody = (): ReactNode => {
    if (!view) return null;
    if (view.from_flags) {
      return <Notice tone="amber" title="Started with flags" body="faber runs on command-line flags, not a file, so there is nothing to edit here." />;
    }
    if (formDirty) {
      return (
        <Notice
          tone="amber"
          title="Form changes not saved"
          body={`You changed ${formChanges.length} ${formChanges.length === 1 ? 'setting' : 'settings'} in the form. Save or discard them before editing the file as text, so the two don’t overwrite each other.`}
          actions={
            <>
              <Button variant="link" disabled={st.saving} onClick={discard}>
                Discard
              </Button>
              <Button variant="primary" loading={st.saving} onClick={() => void save()}>
                Save
              </Button>
            </>
          }
        />
      );
    }
    if (st.raw.loadFailed) {
      return (
        <Notice
          tone="red"
          title="Can’t load config.toml"
          body={st.raw.loadFailed}
          actions={<Button onClick={() => void openRaw()}>Retry</Button>}
        />
      );
    }
    if (!st.raw.file) return <Skeleton />;
    const ro = st.raw.outcome;
    return (
      <>
        <RawEditor text={st.raw.text} loaded={st.raw.file.text} path={st.raw.file.path} error={st.raw.error} onChange={(text) => patchRaw({ text })} />
        {ro?.kind === 'invalid' && (
          <Notice tone="red" title="Not saved: a value faber rejects" body={ro.errors.join(' ')} />
        )}
        {ro?.kind === 'refused' && <RefusedNotices refused={ro.refused} onDiscard={discardRaw} />}
        {ro?.kind === 'stale' && (
          <Notice
            tone="amber"
            title="Changed on disk meanwhile"
            body="config.toml changed since you opened it, so it was not saved. Reload to start from the new file; copy anything you want to keep first."
            actions={<Button onClick={() => void reloadAll()}>Reload</Button>}
          />
        )}
        {ro?.kind === 'failed' && <Notice tone="red" title="Not saved" body={ro.message} />}
      </>
    );
  };

  let body: ReactNode;
  if (st.sessionEnded) {
    // The inline dashboard's words for the same 401 (src/web.rs, #session-expired).
    body = (
      <Notice
        tone="red"
        title="Session ended"
        body={
          <>
            {st.sessionEnded} — run <code className="font-mono">faber dashboard</code> or open it from the menu-bar app.
          </>
        }
      />
    );
  } else if (st.phase === 'loading') body = <Skeleton />;
  else if (st.phase === 'error' || !view) {
    body = (
      <Notice
        tone="red"
        title="Can’t load settings"
        body={st.loadError ?? 'faber didn’t answer on its control socket.'}
        actions={
          <Button
            onClick={() => {
              patch({ phase: 'loading' });
              void refresh();
            }}
          >
            Retry
          </Button>
        }
      />
    );
  } else if (selected === RAW_ID) body = rawBody();
  else if (rawDirty) {
    body = (
      <Notice
        tone="amber"
        title="Text changes not saved"
        body="You edited config.toml as text. Save or discard those changes before editing the form, so the two don’t overwrite each other."
        actions={
          <>
            <Button variant="link" disabled={st.raw.saving} onClick={discardRaw}>
              Discard
            </Button>
            <Button variant="primary" loading={st.raw.saving} onClick={() => void saveRaw()}>
              Save
            </Button>
          </>
        }
      />
    );
  } else if (selected === PROFILES_ID) body = st.profile !== null ? profileView(st.profile) : profileList();
  else body = selected ? sectionBody(selected) : null;

  // ---- the bar: the form's draft, or the text's lines
  const onRaw = selected === RAW_ID;
  let bar: ReactNode = null;
  if (st.sessionEnded) bar = null;
  else if (view && !onRaw && formDirty && !rawDirty) {
    bar = <SaveBar changes={formChanges.map(effectLive)} saving={st.saving} onDiscard={discard} onSave={() => void save()} />;
  } else if (view && onRaw && rawDirty && !formDirty && !view.from_flags) {
    const lines = Math.max(1, changedLines(st.raw.text, st.raw.file?.text ?? '').length);
    const stand: SaveBarChange[] = st.raw.preview?.length ? st.raw.preview.map(effectLive) : [{ key: 'text', label: 'config.toml', live: false }];
    bar = <SaveBar changes={stand} saving={st.raw.saving} summary={rawSummary(lines, st.raw.preview)} onDiscard={discardRaw} onSave={() => void saveRaw()} />;
  }

  return (
    <div className="flex min-h-screen flex-col bg-surface font-sans text-body text-fg narrow:flex-row">
      <SettingsNav
        items={items}
        selected={selected ?? ''}
        onSelect={select}
        query={st.query}
        onQuery={(query) => patch({ query })}
        results={results}
      />
      <main className="relative flex min-w-0 flex-1 flex-col">
        <div className="flex flex-1 flex-col gap-lg px-xl py-xl narrow:px-3xl">
          {st.phase === 'ready' && !st.sessionEnded && notices.length > 0 && <div className="flex flex-col gap-sm">{notices}</div>}
          {body}
        </div>
        {bar && <div className="sticky bottom-lg mx-xl mb-lg narrow:mx-3xl">{bar}</div>}
      </main>
    </div>
  );
}
