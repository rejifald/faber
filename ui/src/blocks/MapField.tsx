import { X } from 'lucide-react';
import { Button, IconButton } from '../kit';
import { sudoCmd } from '../settings/draft';
import { canRemove } from '../settings/lists';
import type { FieldSpec } from '../settings/types';
import { Command } from './TrustRow';

export interface MapFieldProps {
  /** The `map` field. */
  field: FieldSpec;
  /** The entries now, in the draft. */
  entries: Record<string, string>;
  /** The entries as saved, to mark the removed. */
  loaded: Record<string, string>;
  /** Called with the whole new map. */
  onChange: (entries: Record<string, string>) => void;
  /** The profile the field belongs to; its name fills `<profile>` in the command. */
  profile?: { name: string } | null;
  /** The line shown when there are no entries; the field's own words. */
  empty?: string;
}

/** The apply command as the operator types it: the template's `<edited config.toml>` is the file they edit. */
function applyCmd(field: FieldSpec, profile?: { name: string } | null): string {
  return sudoCmd(field.sudo ?? '', profile).replace('<edited config.toml>', 'config.toml');
}

/**
 * A `map` field (a profile's Environment) as rows of name and value. The value is cut with an
 * ellipsis and shows whole on hover. A `sudo` map narrows in place: remove an entry (with Undo)
 * or clear them all; adding or changing one is a command, shown below.
 *
 * @useWhen a NAME=value map in the Settings form. For a list of strings use ListField.
 * @states default, entry removed, empty, every entry removed
 */
export function MapField({ field, entries, loaded, onChange, profile, empty = 'None.' }: MapFieldProps) {
  const names = [...Object.keys(loaded), ...Object.keys(entries).filter((k) => !(k in loaded))];
  const count = Object.keys(entries).length;
  const removable = !field.sudo || field.narrows === 'remove';
  const everyRemoved = count === 0 && Object.keys(loaded).length > 0;
  const keep = (without: string) => {
    const next: Record<string, string> = {};
    for (const n of names) if (n !== without && n in entries) next[n] = entries[n];
    return next;
  };
  const undo = (name: string) => {
    const next: Record<string, string> = {};
    for (const n of names) if (n in entries || n === name) next[n] = n === name ? loaded[n] : entries[n];
    onChange(next);
  };

  return (
    <div className="flex min-w-0 flex-col gap-sm">
      {names.length === 0 ? (
        <p className="text-small text-muted">{empty}</p>
      ) : (
        <ul aria-label={field.label} className="flex flex-col rounded-sm border border-border">
          {names.map((name) => {
            const removed = !(name in entries);
            const at = Object.keys(entries).indexOf(name);
            return (
              <li key={name} className={`flex items-center gap-sm border-b border-border-soft px-md py-2xs last:border-b-0 ${removed ? 'bg-accent-soft' : ''}`}>
                <span className={`w-1/3 shrink-0 font-mono text-small font-medium wrap-anywhere ${removed ? 'text-muted line-through' : 'text-fg-strong'}`}>{name}</span>
                {removed ? (
                  <>
                    <span className="min-w-0 flex-1 text-small text-accent-text">Removed</span>
                    <Button size="sm" variant="link" onClick={() => undo(name)}>
                      Undo
                    </Button>
                  </>
                ) : (
                  <>
                    <span title={entries[name]} className="min-w-0 flex-1 truncate font-mono text-small text-fg-strong">
                      {entries[name]}
                    </span>
                    {removable && canRemove(field, at, count) && <IconButton icon={X} label={`Remove ${name}`} onClick={() => onChange(keep(name))} />}
                  </>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {everyRemoved && (
        <p role="status" className="text-caption text-muted">
          Every entry is removed. That is the same as {field.narrow?.label ?? 'Clear'}.
        </p>
      )}
      {removable && field.narrow && count > 0 && (
        <div>
          <Button size="sm" onClick={() => onChange({})}>
            {field.narrow.label}
          </Button>
        </div>
      )}
      {field.sudo && (
        <div className="flex min-w-0 flex-col gap-2xs">
          <span className="text-caption text-muted">To add or change one: edit config.toml, then</span>
          <Command>{applyCmd(field, profile)}</Command>
        </div>
      )}
    </div>
  );
}
