import { ChevronRight } from 'lucide-react';
import { useId } from 'react';
import { Button, Icon, RowButton } from '../kit';
import { cx } from '../kit/ladder';
import { MARK, RESTORE_ALL, plainText, type AttentionRow, type Part } from '../popover/attention';
import { Pipe } from './Pipe';
import { rowIcons } from './AttentionRow';

/** A dismissed row's words, quieter than a live row's: the head semibold in the list's muted ink. */
function quiet(p: Part, i: number) {
  if (typeof p === 'string') return p;
  if ('strong' in p) return <span key={i} className="font-semibold">{p.strong}</span>;
  if ('mono' in p) return <span key={i} className="font-mono text-caption">{p.mono}</span>;
  return <Pipe key={i} />;
}

export interface DismissedListProps {
  /** The dismissed rows whose cause is still active, worst first. Daemon text is set as text. */
  rows: AttentionRow[];
  /** Whether the list shows. The screen keeps it. */
  open: boolean;
  /** Called with the new state when the line is pressed. */
  onOpenChange: (open: boolean) => void;
  /** Shows a row again: its key, or none for every row. */
  onRestore: (key?: string) => void;
  /** The key whose Restore is in flight, or `RESTORE_ALL`: it shows loading and the others wait. */
  pending?: string;
  /** What the last Restore reported, until it is pressed again. */
  error?: string;
  /** Set on the dashboard, where it stands alone among cards: it draws in a card of its own. */
  card?: boolean;
}

/**
 * The attention rows the operator dismissed while their cause is still active (ADR-0064 § 2),
 * folded into one quiet line, "N dismissed", so nothing disappears silently. Pressed, it opens
 * in place to list each row's words, muted, with its severity mark and Restore; with more than
 * one, Restore all ends the list. No fill, radius or accent: a line of the list it ends.
 *
 * @useWhen after the attention rows (popover) or the alarm cards (dashboard rail, Memory's
 * warnings), only while a dismissed row is still active. Never counted in a severity tally.
 * @states collapsed, open, restore pending, restore failed, in a card
 */
export function DismissedList({ rows, open, onOpenChange, onRestore, pending, error, card }: DismissedListProps) {
  const listId = useId();
  if (!rows.length) return null;
  const busy = (key: string) => pending !== undefined && pending !== key;
  return (
    <div className={cx('flex flex-col', card && 'rounded-lg border border-border bg-surface py-2xs')}>
      <RowButton expanded={open} controls={listId} onClick={() => onOpenChange(!open)}>
        <span className={cx('flex text-faint transition-transform group-hover/row:text-fg motion-reduce:transition-none', open && 'rotate-90')}>
          <Icon icon={ChevronRight} size="sm" />
        </span>
        {`${rows.length} dismissed`}
      </RowButton>
      {open && (
        <ul id={listId} className="flex flex-col gap-2xs pb-xs">
          {rows.map((r) => (
            <li key={r.key} className="flex items-start gap-sm px-xl text-small text-muted">
              <span className={MARK[r.sev]}>
                <Icon icon={rowIcons[r.icon]} size="sm" leading />
              </span>
              <p className="min-w-0 flex-1 line-clamp-2 wrap-anywhere" title={plainText(r.parts)}>
                {r.parts.map(quiet)}
              </p>
              <Button size="sm" variant="link" loading={pending === r.key} disabled={busy(r.key)} onClick={() => onRestore(r.key)}>
                Restore
              </Button>
            </li>
          ))}
          {rows.length > 1 && (
            <li className="flex items-center px-xl">
              <Button size="sm" variant="link" loading={pending === RESTORE_ALL} disabled={busy(RESTORE_ALL)} onClick={() => onRestore()}>
                Restore all
              </Button>
            </li>
          )}
          <li role="status" className="px-xl text-caption text-red empty:hidden">
            {error}
          </li>
        </ul>
      )}
    </div>
  );
}
