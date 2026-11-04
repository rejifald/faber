import type { ReactNode } from 'react';
import { focusRing } from '../kit';
import { RowDisclosure } from '../kit/RowDisclosure';

/** One column: its header, how wide it is and how its cells are set. */
export interface DataTableColumn {
  /** Names the cell in each row's `cells`. */
  key: string;
  /** The column's header, in sentence case. */
  header: string;
  /** `auto` and `tight` size to their content without wrapping or clipping; `fill` takes the width that is left and clips long text with an ellipsis. Default `auto`. */
  width?: 'auto' | 'tight' | 'fill';
  /** `end` right-aligns the column, for numbers. Default `start`. */
  align?: 'start' | 'end';
  /** Sets the column's cells in the mono face with tabular numerals. */
  mono?: boolean;
}

/** One row: its cells by column key, an optional wash and optional detail under it. */
export interface DataTableRow {
  /** A stable identity for the row. */
  key: string;
  /** The cell content by column key. Daemon text is passed as strings and set as text. */
  cells: Record<string, ReactNode>;
  /** Washes the row in the soft tint of a caution or a fault. */
  tone?: 'amber' | 'red';
  /** Draws an amber or red bar at the row's lead edge, without the wash: a retried run, a parked or frozen job. A selected row's accent bar wins. */
  bar?: 'amber' | 'red';
  /** Detail shown under the row across all columns, opened by a toggle in the lead column; it wraps where the cells clip. */
  detail?: ReactNode;
  /** The toggle's accessible name, e.g. `Details for acme/mobile build`. Default `Details`. */
  detailLabel?: string;
  /** Whether the detail shows at first. Default shut. */
  defaultOpen?: boolean;
}

export interface DataTableProps {
  /** The columns, in order. */
  columns: DataTableColumn[];
  /** The rows, in order. */
  rows: DataTableRow[];
  /** Names the table for assistive tech; not shown. */
  caption: string;
  /** The words shown in place of the rows when there are none. */
  empty: string;
  /** The key of the selected row; it gets the pressed wash and an accent lead bar. Only meaningful with `onSelect`. */
  selectedKey?: string;
  /** Makes each row pressable as a whole (click, Enter or Space) and calls this with its key. The table is then announced as a grid. A row's detail toggle does not select. Without it the table is not interactive. */
  onSelect?: (key: string) => void;
}

const WIDTH = {
  auto: 'whitespace-nowrap',
  tight: 'w-px whitespace-nowrap',
  fill: 'w-full max-w-0 whitespace-nowrap',
};
const TONE = { amber: 'bg-amber-soft', red: 'bg-red-soft' };
const BAR = { amber: 'bg-amber', red: 'bg-red' };

/**
 * A dense table of rows, one per job or per run: left-aligned text, right-aligned mono numbers,
 * a hairline between rows, a soft wash on a row that needs a look, a lead bar to flag one, and its detail under it.
 *
 * @useWhen listing jobs in flight or recent activity, where the operator scans down a column.
 * Not for a few name and value facts (KeyValueList) and not for settings (FieldRow).
 * With `onSelect` a row is pressable as a whole and the selected one is marked, for a table that
 * picks one row to show elsewhere.
 *
 * @states default, toned row, barred row, expanded row, closed detail row, empty, long text, selectable, selected row
 */
export function DataTable({ columns, rows, caption, empty, selectedKey, onSelect }: DataTableProps) {
  const expandable = rows.some((r) => r.detail !== undefined);
  const span = columns.length + (expandable ? 1 : 0);
  return (
    <table role={onSelect ? 'grid' : undefined} className="w-full border-collapse text-left text-body text-fg">
      <caption className="sr-only">{caption}</caption>
      <thead>
        <tr>
          {expandable && (
            <th scope="col" className="w-px px-lg pb-sm">
              <span className="sr-only">Details</span>
            </th>
          )}
          {columns.map((c) => (
            <th
              key={c.key}
              scope="col"
              className={`px-lg pb-sm text-caption font-semibold text-muted ${c.align === 'end' ? 'text-right' : 'text-left'} ${c.width === 'tight' ? 'whitespace-nowrap' : ''}`}
            >
              {c.header}
            </th>
          ))}
        </tr>
      </thead>
      {rows.length === 0 && (
        <tbody>
          <tr>
            <td colSpan={span} className="border-t border-border-soft px-lg py-md text-small text-muted">
              {empty}
            </td>
          </tr>
        </tbody>
      )}
      {rows.map((row) => {
        const selected = onSelect !== undefined && row.key === selectedKey;
        const barClass = selected ? 'bg-accent' : row.bar ? BAR[row.bar] : null;
        const bar = barClass ? <span aria-hidden className={`absolute inset-y-0 left-0 w-3xs ${barClass}`} /> : null;
        const lead = 'relative border-t border-border-soft px-lg py-md';
        const summary = (toggle: ReactNode) => (
          <tr
            className={`${selected ? 'bg-row-press' : row.tone ? TONE[row.tone] : ''} ${onSelect ? `cursor-pointer ${focusRing} ${selected ? '' : 'hover:bg-row-hover'}` : ''}`}
            tabIndex={onSelect ? 0 : undefined}
            aria-selected={onSelect ? selected : undefined}
            onClick={onSelect ? () => onSelect(row.key) : undefined}
            onKeyDown={
              onSelect
                ? (e) => {
                    if (e.target === e.currentTarget && (e.key === 'Enter' || e.key === ' ')) {
                      e.preventDefault();
                      onSelect(row.key);
                    }
                  }
                : undefined
            }
          >
            {expandable && (
              <td className={`w-px ${lead}`}>
                {bar}
                <span onClick={(e) => e.stopPropagation()}>{toggle}</span>
              </td>
            )}
            {columns.map((c, i) => (
              <td
                key={c.key}
                className={`overflow-hidden text-ellipsis ${lead} ${WIDTH[c.width ?? 'auto']} ${c.align === 'end' ? 'text-right' : 'text-left'} ${c.mono ? 'font-mono tabular-nums' : ''}`}
              >
                {i === 0 && !expandable && bar}
                {row.cells[c.key]}
              </td>
            ))}
          </tr>
        );
        return row.detail === undefined ? (
          <tbody key={row.key}>{summary(null)}</tbody>
        ) : (
          <RowDisclosure
            key={row.key}
            label={row.detailLabel ?? 'Details'}
            defaultOpen={row.defaultOpen}
            colSpan={span}
            tone={row.tone}
            row={summary}
            detail={row.detail}
          />
        );
      })}
    </table>
  );
}
