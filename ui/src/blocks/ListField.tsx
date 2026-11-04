import { X } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Button, Field, IconButton, Input } from '../kit';
import { sudoCmd } from '../settings/draft';
import { canAdd, canRemove, validateItem } from '../settings/lists';
import type { FieldSpec } from '../settings/types';
import { Command } from './TrustRow';

export interface ListFieldProps {
  /** The `csv` or `lines` field. */
  field: FieldSpec;
  /** The items now, in the draft. */
  items: string[];
  /** The items as saved, to mark the added and the removed. */
  loaded: string[];
  /** Called with the whole new list. */
  onChange: (items: string[]) => void;
  /** The profile the field belongs to; its name fills `<profile>` in the commands. */
  profile?: { name: string } | null;
  /** The line shown when there are no items; the field's own words. */
  empty?: string;
}

interface Row {
  text: string;
  state: 'kept' | 'removed' | 'added';
}

/** Loaded items first (removed ones marked), then the added: the order the list shows. */
function rowsOf(items: string[], loaded: string[]): Row[] {
  const rows: Row[] = loaded.map((text) => ({ text, state: items.includes(text) ? 'kept' : 'removed' }));
  for (const text of items) if (!loaded.includes(text)) rows.push({ text, state: 'added' });
  return rows;
}

/** The command that empties the field: its template with the value placeholder made `""`. */
function clearCmd(field: FieldSpec, profile?: { name: string } | null): string {
  return sudoCmd(field.sudo ?? '', profile).replace(/<[^>]*>$/, '""');
}

/**
 * A `csv` or `lines` field as a bordered list, one item per row, edited per item: a plain list
 * adds and removes freely; a `sudo` list only in the direction its `narrows` allows, and shows the
 * command for the other. An item removed or added is marked until saved, with Undo.
 *
 * @useWhen a list of strings in the Settings form (events, actors, priority repos, dropped grants,
 * labels in a new profile). For name and value pairs use MapField.
 * @states default, item removed, item added, invalid entry, duplicate, empty, last item refused, every item removed
 */
export function ListField({ field, items, loaded, onChange, profile, empty = 'None.' }: ListFieldProps) {
  const [draft, setDraft] = useState('');
  const [refused, setRefused] = useState(false);
  const list = useRef<HTMLUListElement>(null);
  const focusAfter = useRef<number | null>(null);

  const rows = rowsOf(items, loaded);
  const kept = rows.filter((r) => r.state !== 'removed').map((r) => r.text);
  const mayAdd = canAdd(field);
  const sudo = field.sudo !== undefined;
  const text = draft.trim();
  const duplicate = text !== '' && kept.includes(text);
  const invalid = text !== '' && !duplicate ? validateItem(field, text, kept) : null;

  useEffect(() => {
    if (focusAfter.current === null) return;
    const buttons = list.current?.querySelectorAll<HTMLButtonElement>('button[data-remove]');
    buttons?.[Math.min(focusAfter.current, buttons.length - 1)]?.focus();
    focusAfter.current = null;
  }, [items]);

  const emit = (next: Row[]) => onChange(next.filter((r) => r.state !== 'removed').map((r) => r.text));
  const mark = (at: number, state: Row['state']) => emit(rows.map((r, i) => (i === at ? { ...r, state } : r)));
  const remove = (at: number) => {
    const keptAt = kept.indexOf(rows[at].text);
    if (!canRemove(field, keptAt, kept.length)) {
      setRefused(true);
      return;
    }
    setRefused(false);
    focusAfter.current = keptAt;
    mark(at, 'removed');
  };
  const add = () => {
    if (!text || duplicate || invalid) return;
    onChange([...kept, text]);
    setDraft('');
    setRefused(false);
  };

  const everyRemoved = field.narrows === 'remove' && kept.length === 0 && loaded.length > 0;
  const removable = !sudo || field.narrows === 'remove' || field.narrows === 'remove-keep-one';

  return (
    <div className="flex min-w-0 flex-col gap-sm">
      {rows.length === 0 ? (
        <p className="text-small text-muted">{empty}</p>
      ) : (
        <ul ref={list} aria-label={field.label} className="flex flex-col rounded-sm border border-border">
          {rows.map((r, i) => (
            <li
              key={`${r.state === 'added' ? 'a' : 'l'}:${r.text}`}
              className={`flex items-center gap-sm border-b border-border-soft px-md py-2xs last:border-b-0 ${r.state === 'kept' ? '' : 'bg-accent-soft'}`}
            >
              <span className={`min-w-0 flex-1 font-mono text-small wrap-anywhere ${r.state === 'removed' ? 'text-muted line-through' : 'text-fg-strong'}`}>{r.text}</span>
              {r.state === 'kept' && removable && <IconButton icon={X} label={`Remove ${r.text}`} data-remove="" onClick={() => remove(i)} />}
              {r.state === 'removed' && (
                <>
                  <span className="text-caption text-accent-text">Removed</span>
                  <Button size="sm" variant="link" onClick={() => mark(i, 'kept')}>
                    Undo
                  </Button>
                </>
              )}
              {r.state === 'added' && (
                <>
                  <span className="text-caption text-accent-text">Added</span>
                  <Button size="sm" variant="link" onClick={() => emit(rows.filter((_, at) => at !== i))}>
                    Undo
                  </Button>
                </>
              )}
            </li>
          ))}
        </ul>
      )}
      {refused && field.sudo && (
        <p role="status" className="text-caption text-amber">
          Removing the last one trusts everyone, which widens. Use <code className="font-mono">{clearCmd(field, profile)}</code> if you mean it.
        </p>
      )}
      {everyRemoved && (
        <p role="status" className="text-caption text-amber">
          {`No ${field.label.toLowerCase()}: faber will refuse every job once this applies.`}
        </p>
      )}
      {mayAdd && (
        <form
          className="flex items-start gap-sm"
          onSubmit={(e) => {
            e.preventDefault();
            add();
          }}
        >
          <div className="min-w-0 flex-1">
            <Field label={<span className="sr-only">{`Add to ${field.label}`}</span>} error={invalid ?? undefined}>
              <Input mono value={draft} onValueChange={setDraft} placeholder={field.item?.example} />
            </Field>
            {duplicate && (
              <p role="status" className="pt-2xs text-caption text-amber">
                {`${text} is already listed.`}
              </p>
            )}
          </div>
          <Button type="submit">Add</Button>
        </form>
      )}
      {sudo && field.sudo && (
        <div className="flex min-w-0 flex-col gap-2xs">
          <span className="text-caption text-muted">{field.narrows === 'add' ? 'To remove one:' : field.narrows ? 'To add one:' : 'To change it:'}</span>
          <Command>{sudoCmd(field.sudo, profile)}</Command>
        </div>
      )}
    </div>
  );
}
