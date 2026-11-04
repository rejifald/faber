import type { ReactNode } from 'react';
import { Sparkline, type SparklineTone } from '../kit';

export interface StatTileProps {
  /** The question the number answers, in sentence case: "Delivered". */
  label: string;
  /** The number, with its optional unit. Pass "—" while it loads. */
  value: ReactNode;
  /** The one-line reading under the number. */
  note?: ReactNode;
  /** Colours the value: `amber` for caution, `red` for a fault. Default `neutral`. */
  tone?: 'neutral' | 'amber' | 'red';
  /** `sm` sets the value at title size and the note on up to two lines, for a few boxes in a detail panel. Default `md`. */
  size?: 'md' | 'sm';
  /** The recent history drawn beside the value; `label` is its text summary for assistive tech. */
  trend?: { values: number[]; tone?: SparklineTone; label: string };
}

const VALUE_TONE = { neutral: 'text-fg-strong', amber: 'text-amber', red: 'text-red' } as const;

/**
 * One number that answers one question, with its trend and a one-line reading.
 *
 * @useWhen a dashboard row of headline figures, each a single measure over a window. Not for a
 * list of facts about one thing; use KeyValueList.
 * @states default, toned, with trend, no trend, loading, small
 */
export function StatTile({ label, value, note, tone = 'neutral', size = 'md', trend }: StatTileProps) {
  return (
    <div className={`flex min-w-0 flex-col gap-xs rounded-lg border border-border bg-surface ${size === 'sm' ? 'px-lg py-md' : 'px-xl py-lg'}`}>
      <div className="text-caption font-semibold text-muted">{label}</div>
      <div className="flex items-end justify-between gap-md">
        <div className={`font-mono ${size === 'sm' ? 'text-title' : 'text-display'} font-semibold tabular-nums ${VALUE_TONE[tone]}`}>{value}</div>
        {trend && <Sparkline values={trend.values} tone={trend.tone} label={trend.label} />}
      </div>
      {note && <div className={`text-caption text-muted ${size === 'sm' ? 'line-clamp-2' : ''}`}>{note}</div>}
    </div>
  );
}
