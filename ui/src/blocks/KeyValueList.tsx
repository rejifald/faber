import type { ReactNode } from 'react';
import { StatusDot, type StatusTone } from '../kit';

/** One fact: a name, its value, and optionally a state dot and a note. */
export interface KeyValueItem {
  /** A stable identity for the row. */
  key: string;
  /** The name of the fact. Truncates when long. */
  label: ReactNode;
  /** The fact. Daemon text is passed as strings and set as text. */
  value: ReactNode;
  /** The row's state: a dot beside the label, and amber or red also colours the value. */
  tone?: StatusTone;
  /** A quiet line under the row. */
  note?: ReactNode;
}

export interface KeyValueListProps {
  /** The facts, in order. */
  items: KeyValueItem[];
  /** Which side is set in the mono face: for identifiers and numbers. Default none. */
  mono?: 'label' | 'value' | 'both';
  /** `rows`: one fact per line, the value right-aligned. `grid`: each fact as its label over its value, in columns that wrap. Default `rows`. */
  layout?: 'rows' | 'grid';
}

const VALUE_TONE: Partial<Record<StatusTone, string>> = {
  amber: 'font-semibold text-amber',
  red: 'font-semibold text-red',
};

/**
 * A short list of name and value facts, one per line, the value right-aligned, optionally with
 * a state dot beside the name. When any row has a dot, every row reserves its column, so the
 * labels of the rows without one stay in line.
 *
 * @useWhen showing a host's health or the learned caps: a handful of facts read at a glance.
 * Not for rows with several columns (DataTable) and not for editable settings (FieldRow).
 * `layout="grid"` sets each fact as a small label over its mono value, for the detail of one thing.
 *
 * @states default, with dots, toned value, long label, grid
 */
export function KeyValueList({ items, mono, layout = 'rows' }: KeyValueListProps) {
  const monoLabel = mono === 'label' || mono === 'both';
  const monoValue = mono === 'value' || mono === 'both';
  if (layout === 'grid') {
    return (
      <dl className="flex flex-wrap gap-x-3xl gap-y-md">
        {items.map((item) => (
          <div key={item.key} className="flex min-w-0 flex-col gap-3xs">
            <dt className="text-caption text-faint">{item.label}</dt>
            <dd className={`font-mono text-small wrap-anywhere ${(item.tone && VALUE_TONE[item.tone]) || 'text-fg'}`}>{item.value}</dd>
            {item.note !== undefined && <dd className="text-caption text-faint">{item.note}</dd>}
          </div>
        ))}
      </dl>
    );
  }
  // The dot column is reserved for every row once any row has a dot, so the labels line up.
  const dotted = items.some((item) => item.tone);
  return (
    <dl className="flex flex-col">
      {items.map((item) => (
        <div key={item.key} className="flex flex-wrap items-baseline gap-sm py-2xs text-small">
          {item.tone ? <StatusDot tone={item.tone} /> : dotted ? <span aria-hidden className="size-sm shrink-0" /> : null}
          <dt className={`min-w-0 flex-1 truncate ${monoLabel ? 'font-mono text-fg' : 'text-muted'}`}>{item.label}</dt>
          <dd className={`text-right ${monoValue ? 'font-mono' : ''} ${(item.tone && VALUE_TONE[item.tone]) || 'text-fg'}`}>
            {item.value}
          </dd>
          {item.note !== undefined && <dd className="basis-full text-caption text-faint">{item.note}</dd>}
        </div>
      ))}
    </dl>
  );
}
