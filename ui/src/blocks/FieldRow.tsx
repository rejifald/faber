import { useId, useState, type ReactNode } from 'react';
import { Input, Select, Switch } from '../kit';
import { asText, readValue } from '../settings/draft';
import type { Config, FieldSpec } from '../settings/types';
import { ListField } from './ListField';
import { MapField } from './MapField';
import { TrustRow } from './TrustRow';

export interface FieldRowProps {
  /** The field's spec; its kind says which control the row draws. */
  field: FieldSpec;
  /** The draft value, in the config's shape (a number, a bool, an array, an object). */
  value: unknown;
  /** The value as saved: a field that differs from it is `changed`, and a list marks its added and removed items. */
  loaded?: unknown;
  /** The daemon's defaults, for a blank required number. */
  defaults?: Config;
  /** The profile the field belongs to; its name fills `<profile>` in commands. */
  profile?: { name: string } | null;
  /** The daemon's or the validator's message, shown under the control. */
  error?: string;
  /** `inline`: label column, then control column (the control at the right). `stacked`: label and hint above the control. */
  layout?: 'inline' | 'stacked';
  /** Shows the `live` or `restart` tag; a section that says so once in its hint turns it off. A `changed` tag always shows. */
  tagged?: boolean;
  /** Called with the new value in the config's shape. */
  onChange: (value: unknown) => void;
}

const UNSET = 'unset';
const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const strings = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);
const record = (v: unknown): Record<string, string> =>
  v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, String(x)])) : {};

/**
 * The text of a number or text input, kept while it is typed: "1." and a blank are not values yet,
 * so the text is only taken from `value` again when `value` changed from outside what was typed.
 */
function useTyped(value: unknown, show: (v: unknown) => string): [string, (text: string, emitted: unknown) => void] {
  const [state, setState] = useState({ text: show(value), emitted: value });
  if (!same(state.emitted, value)) setState({ text: show(value), emitted: value });
  return [state.text, (text, emitted) => setState({ text, emitted })];
}

function ScalarInput({ field, value, defaults, loaded, onChange }: Pick<FieldRowProps, 'field' | 'value' | 'defaults' | 'loaded' | 'onChange'>) {
  const [text, setTyped] = useTyped(value, (v) => asText(field, v));
  const numeric = field.kind === 'int' || field.kind === 'float';
  return (
    <div className="flex items-center gap-xs">
      <div className="min-w-0 flex-1">
        <Input
          mono
          type={numeric ? 'number' : 'text'}
          value={text}
          onValueChange={(t) => {
            const next = readValue(field, t, loaded, defaults);
            setTyped(t, next);
            onChange(next);
          }}
        />
      </div>
      {field.unit && <span className="text-caption text-muted">{field.unit}</span>}
    </div>
  );
}

function Ceiling({ field, value, onChange }: Pick<FieldRowProps, 'field' | 'value' | 'onChange'>) {
  const mode = typeof value === 'number' ? 'number' : value === 'off' ? 'off' : 'auto';
  const [text, setTyped] = useTyped(value, (v) => (typeof v === 'number' ? String(v) : ''));
  return (
    <div className="flex flex-col gap-xs">
      <Select
        value={mode}
        options={[
          { value: 'auto', label: 'Auto' },
          { value: 'off', label: 'Off' },
          { value: 'number', label: 'Number' },
        ]}
        onValueChange={(m) => onChange(m === 'number' ? (typeof value === 'number' ? value : (field.min ?? 1)) : m)}
      />
      {mode === 'number' && (
        <Input
          mono
          type="number"
          value={text}
          onValueChange={(t) => {
            const n = Number(t);
            if (t.trim() !== '' && Number.isFinite(n) && n > 0) {
              setTyped(t, Math.round(n));
              onChange(Math.round(n));
            } else setTyped(t, value);
          }}
        />
      )}
    </div>
  );
}

function control(p: FieldRowProps): ReactNode {
  const { field, value, loaded, defaults, onChange, profile } = p;
  if ((field.sudo || field.held) && !field.narrows) {
    const changed = !same(value, loaded);
    return (
      <TrustRow
        field={field}
        value={loaded}
        profile={profile}
        pending={changed ? value : undefined}
        onNarrow={() => field.narrow && onChange(JSON.parse(field.narrow.value))}
        onUndo={() => onChange(loaded)}
      />
    );
  }
  switch (field.kind) {
    case 'csv':
    case 'lines':
      return <ListField field={field} items={strings(value)} loaded={strings(loaded)} onChange={onChange} profile={profile} />;
    case 'map':
      return <MapField field={field} entries={record(value)} loaded={record(loaded)} onChange={onChange} profile={profile} />;
    case 'bool':
      if (!field.optional) return <div className="flex justify-end"><Switch label={field.label} checked={value === true} onCheckedChange={onChange} /></div>;
      return (
        <Select
          value={value == null ? UNSET : value ? 'true' : 'false'}
          options={(field.options ?? ['not stated', 'yes', 'no']).map((label, i) => ({ value: [UNSET, 'true', 'false'][i], label }))}
          onValueChange={(v) => onChange(v === UNSET ? null : v === 'true')}
        />
      );
    case 'enum': {
      const options = (field.options ?? []).map((o) => ({ value: o, label: o }));
      if (field.optional) options.unshift({ value: UNSET, label: 'unstated' });
      return <Select value={value == null || value === '' ? UNSET : String(value)} options={options} onValueChange={(v) => onChange(v === UNSET ? null : v)} />;
    }
    case 'ceiling':
      return <Ceiling field={field} value={value} onChange={onChange} />;
    default:
      return <ScalarInput field={field} value={value} loaded={loaded} defaults={defaults} onChange={onChange} />;
  }
}

/**
 * One field of the Settings form: its label and hint, a `live` or `restart` tag (or `changed`), and
 * the control its kind calls for. A list goes to ListField, a map to MapField, and a `sudo` or
 * `held` field that may not narrow in place to TrustRow. A changed field draws its control with the accent
 * border; an invalid one shows its message under the control.
 *
 * @useWhen every field of a SectionSpec or a profile, rendered from its FieldSpec. Never one
 * hard-coded per key.
 * @states default, changed, invalid, live, restart
 */
export function FieldRow(props: FieldRowProps) {
  const { field, value, loaded, error, layout = 'inline', tagged = true } = props;
  const id = useId();
  const changed = !same(value, loaded);
  const wide = field.kind === 'csv' || field.kind === 'lines' || field.kind === 'map' || ((field.sudo !== undefined || field.held === true) && !field.narrows);
  const scalar = !wide;
  // A `sudo` or held field draws no tag: its pending row (Removed, Unsaved) already says it changed (Trust.png).
  const tag = field.sudo || field.held ? null : changed ? (
    <span className="text-caption text-accent-text">changed</span>
  ) : tagged ? (
    <span className={`text-caption ${field.live ? 'text-green' : 'text-amber'}`}>{field.live ? 'live' : 'restart'}</span>
  ) : null;
  const labelBlock = (
    <div className="flex min-w-0 flex-col gap-2xs">
      <span id={id} className="text-small font-medium text-fg">
        {field.label}
      </span>
      {field.hint && <span className="text-caption text-muted">{field.hint}</span>}
      {(wide || layout === 'stacked') && tag}
    </div>
  );
  const box = (
    <div role="group" aria-labelledby={id} className="flex min-w-0 flex-col gap-2xs">
      {scalar && changed && field.kind !== 'bool' ? (
        <div className="rounded-sm outline-solid outline-2 outline-accent">{control(props)}</div>
      ) : (
        control(props)
      )}
      {error && (
        <p role="alert" className="text-caption text-red">
          {error}
        </p>
      )}
    </div>
  );
  if (layout === 'stacked') {
    return (
      <div className="flex flex-col gap-sm border-b border-border-soft py-lg">
        {labelBlock}
        <div className={scalar ? 'w-field' : ''}>{box}</div>
      </div>
    );
  }
  return (
    <div className="flex items-start gap-lg border-b border-border-soft py-lg">
      <div className={wide ? 'w-label shrink-0' : 'min-w-0 flex-1'}>{labelBlock}</div>
      {scalar && tag}
      <div className={wide ? 'min-w-0 flex-1' : 'w-field shrink-0'}>{box}</div>
    </div>
  );
}
