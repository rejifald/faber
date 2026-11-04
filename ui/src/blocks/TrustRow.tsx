import { Copy } from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Button, IconButton } from '../kit';
import { narrowShown, sudoCmd } from '../settings/draft';
import type { FieldSpec } from '../settings/types';

/** The text a trust value reads as: a list or map joined, `none` when empty, a bool as the field words it. */
export function trustText(field: FieldSpec, v: unknown): string {
  if (v == null || v === '') return field.options && field.kind === 'bool' ? (field.options[0] ?? 'not stated') : 'not set';
  if (typeof v === 'boolean') return field.options && field.options.length >= 3 ? (v ? field.options[1] : field.options[2]) : v ? 'on' : 'off';
  if (Array.isArray(v)) return v.length ? v.join(', ') : 'none';
  if (typeof v === 'object') {
    const e = Object.entries(v as Record<string, unknown>);
    return e.length ? e.map(([k, x]) => `${k}=${x}`).join(', ') : 'none';
  }
  return String(v);
}

export interface CommandProps {
  /** The command, built by `sudoCmd`; it wraps and is never cut, since the operator copies it. */
  children: ReactNode;
}

/**
 * A command in a code element that wraps onto as many lines as it needs, breaking anywhere: an
 * operator copies it, so it is never cut (settings/anatomy.md § 5, TrustRow).
 *
 * @useWhen a sudo command beside a setting the form may not change. For any other code use plain text.
 * @states default, long (wraps)
 */
export function Command({ children }: CommandProps) {
  return (
    <code className="block min-w-0 rounded-xs bg-surface-2 px-sm py-2xs font-mono text-small text-fg wrap-anywhere whitespace-pre-wrap">
      {children}
    </code>
  );
}

export interface TrustRowProps {
  /** The `sudo` or `held` field. */
  field: FieldSpec;
  /** The value as saved. */
  value: unknown;
  /** The profile the field belongs to; its name fills `<profile>` in the command. */
  profile?: { name: string } | null;
  /** The narrowed value, unsaved; shows the change with its Undo. `null` is a value, `undefined` is none. */
  pending?: unknown;
  /** Makes the field's one narrowing (its `narrow.value`). */
  onNarrow?: () => void;
  /** Takes the pending narrowing back. */
  onUndo?: () => void;
}

/**
 * A `sudo` or `held` field the form may not edit freely: its value, the one-click narrowing when it
 * would narrow, and otherwise the command that changes it, to copy (none for a held field, whose
 * section validation refuses on). The command wraps, never truncates.
 *
 * @useWhen a `sudo` or `held` scalar, or a list or map without `narrows`, inside a FieldRow. For a
 * trust list the form may narrow in place, use ListField or MapField.
 * @states at rest, narrowing offered, narrowing pending, command copied, held (no command)
 */
export function TrustRow({ field, value, profile, pending, onNarrow, onUndo }: TrustRowProps) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const isPending = pending !== undefined;
  const offer = !isPending && narrowShown(field, value);
  const command = field.sudo ? sudoCmd(field.sudo, profile) : undefined;
  const copy = () => {
    if (!command || typeof navigator === 'undefined' || !navigator.clipboard) return;
    navigator.clipboard.writeText(command).then(
      () => {
        setCopied(true);
        clearTimeout(timer.current);
        timer.current = setTimeout(() => setCopied(false), 2000);
      },
      () => {},
    );
  };

  return (
    <div className={`flex min-w-0 flex-col gap-2xs rounded-sm ${isPending ? 'bg-accent-soft p-sm' : ''}`}>
      <div className="flex items-start gap-sm">
        <div className="flex min-w-0 flex-1 flex-col gap-2xs">
          {isPending ? (
            <span className="font-mono text-small text-fg-strong wrap-anywhere">
              <s className="text-muted">{trustText(field, value)}</s> → {trustText(field, pending)}
            </span>
          ) : (
            <span className="font-mono text-small text-fg-strong wrap-anywhere">{trustText(field, value)}</span>
          )}
          {isPending && <span className="text-caption text-accent-text">Unsaved. Narrows, no sudo needed.</span>}
        </div>
        {isPending && (
          <Button size="md" onClick={() => onUndo?.()}>
            Undo
          </Button>
        )}
        {offer && field.narrow && (
          <Button size="md" onClick={() => onNarrow?.()}>
            {field.narrow.label}
          </Button>
        )}
      </div>
      {!isPending && !offer && command && (
        <div className="flex items-start gap-sm">
          <div className="min-w-0 flex-1">
            <Command>{command}</Command>
          </div>
          <IconButton icon={Copy} label="Copy command" size="sm" onClick={copy} />
        </div>
      )}
      <span role="status" className="text-caption text-green empty:hidden">
        {copied ? 'Copied' : ''}
      </span>
    </div>
  );
}
