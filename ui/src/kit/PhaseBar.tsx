import { Bar } from '@visx/shape';
import { scaleLinear } from '@visx/scale';
import { useId } from 'react';

import { cx } from './ladder';
import { useMeasuredWidth } from './useMeasuredWidth';

// The drawing space, in CSS pixels: the svg is drawn at its container's measured width (360 before
// the first measurement). A 2 px gap separates segments, so two of one tone still read as two;
// a segment is never drawn narrower than 2 px. Geometry inside the drawn mark (guidelines § Scales).
const FALLBACK_W = 360;
const H = 12;
const COMPACT_H = 6;
const GAP = 2;
const MIN_SEG = 2;
// Parked carries a diagonal hatch over its amber, in both themes, so it never reads as the
// accent's setup beside it (owner, 2026-10-30): 4 px tiles, a 1.5 px line in the surface ink.
const HATCH = 4;
const HATCH_LINE = 1.5;

/** Who a segment's time belongs to; each is a fill from the theme. */
export type PhaseTone = 'wait' | 'overhead' | 'faber' | 'setup' | 'work' | 'post';

/** The tone drawn hatched rather than flat. */
const HATCHED = 'wait' satisfies PhaseTone;

const FILL: Record<PhaseTone, string> = {
  wait: 'fill-amber',
  overhead: 'fill-faint',
  faber: 'fill-muted',
  setup: 'fill-accent',
  work: 'fill-fg',
  post: 'fill-border-control',
};

const SWATCH: Record<Exclude<PhaseTone, typeof HATCHED>, string> = {
  overhead: 'bg-faint',
  faber: 'bg-muted',
  setup: 'bg-accent',
  work: 'bg-fg',
  post: 'bg-border-control',
};

/** One stretch of a run. */
export interface PhaseSegment {
  key: string;
  label: string;
  /** Its length in ms, or null when it was not measured: then it is listed, never drawn. */
  ms: number | null;
  tone: PhaseTone;
  /** What the stretch is, for the legend. */
  note?: string;
  /** It crosses clocks and came out negative, so it reads as zero. */
  skewed?: boolean;
}

export interface PhaseBarProps {
  /** The stretches of the run, in order. */
  segments: PhaseSegment[];
  /** Formats a length for the legend and the summary. */
  format: (ms: number | null) => string;
  /** A thin bar with no legend, for a table cell. */
  compact?: boolean;
  /** The second figure per segment, shown after the first in the legend (a class's p90). */
  secondary?: Record<string, string>;
}

/**
 * A run's time as one horizontal bar of its phases, in order, each as wide as its share of the
 * measured whole and filled by whose time it was: hatched amber waiting for capacity, greys for faber's
 * and GitHub's overhead, the accent for the job's setup, strong ink for its steps. A phase that was
 * not measured is left out of the bar and listed as "not measured", never drawn as zero. The
 * drawing is hidden from assistive tech; the legend (or, compact, the label) is the text.
 *
 * @useWhen showing where one run's or one job class's time went (Activity's row detail, the Jobs view).
 * For memory peaks use RunStrip or PeakHistogram.
 * @states full, compact, unmeasured phases, skewed phase, nothing measured
 */
export function PhaseBar({ segments, format, compact = false, secondary }: PhaseBarProps) {
  const [box, W] = useMeasuredWidth<HTMLDivElement>(FALLBACK_W);
  // An svg `url(#…)` reference needs an id without React's punctuation.
  const hatch = `phase-hatch-${useId().replace(/[^a-zA-Z0-9]/g, '')}`;
  const drawn = segments.filter((s): s is PhaseSegment & { ms: number } => s.ms !== null && s.ms > 0);
  const total = drawn.reduce((n, s) => n + s.ms, 0);
  const gaps = Math.max(drawn.length - 1, 0) * GAP;
  const x = scaleLinear<number>({ domain: [0, Math.max(total, 1)], range: [0, Math.max(W - gaps, 0)] });
  const h = compact ? COMPACT_H : H;

  const widths = drawn.map((s) => Math.max(MIN_SEG, x(s.ms)));
  const bars = drawn.map((s, i) => ({ s, w: widths[i], x: widths.slice(0, i).reduce((n, w) => n + w + GAP, 0) }));

  const unmeasured = segments.filter((s) => s.ms === null).length;
  const summary =
    drawn.length === 0
      ? 'No phase measured'
      : `${drawn.map((s) => `${s.label} ${format(s.ms)}`).join(', ')}${unmeasured ? `; ${unmeasured} not measured` : ''}`;

  return (
    <div className="flex w-full min-w-0 flex-col gap-sm">
      <div ref={box} role="img" aria-label={summary} className="w-full">
        <svg aria-hidden width={W} height={h} className="block">
          <defs>
            <pattern id={hatch} width={HATCH} height={HATCH} patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
              <rect width={HATCH} height={HATCH} className={FILL[HATCHED]} />
              <line x1={0} y1={0} x2={0} y2={HATCH} strokeWidth={HATCH_LINE} className="stroke-surface" />
            </pattern>
          </defs>
          {drawn.length === 0 && <Bar x={0} y={0} width={W} height={h} className="fill-surface-2" />}
          {bars.map(({ s, x: bx, w }) =>
            s.tone === HATCHED ? (
              <Bar key={s.key} x={bx} y={0} width={w} height={h} fill={`url(#${hatch})`} />
            ) : (
              <Bar key={s.key} x={bx} y={0} width={w} height={h} className={FILL[s.tone]} />
            ),
          )}
        </svg>
      </div>
      {!compact && (
        <ul className="grid grid-cols-1 gap-x-xl gap-y-2xs text-small narrow:grid-cols-2">
          {segments.map((s) => (
            <li key={s.key} className="flex min-w-0 items-baseline gap-sm">
              {s.tone === HATCHED ? (
                <svg aria-hidden viewBox={`0 0 ${HATCH * 3} ${HATCH * 3}`} className="size-icon-sm shrink-0 self-center rounded-xs">
                  <rect width={HATCH * 3} height={HATCH * 3} fill={`url(#${hatch})`} />
                </svg>
              ) : (
                <span aria-hidden className={cx('size-icon-sm shrink-0 self-center rounded-xs', SWATCH[s.tone])} />
              )}
              <span className="shrink-0 text-fg">{s.label}</span>
              <span className={cx('shrink-0 font-mono', s.ms === null ? 'text-faint' : 'text-fg')}>
                {format(s.ms)}
                {secondary?.[s.key] !== undefined && <span className="text-faint">, p90 {secondary[s.key]}</span>}
              </span>
              {s.skewed && <span className="shrink-0 text-caption text-amber">clocks disagree</span>}
              {s.note && <span className="min-w-0 truncate text-caption text-faint">{s.note}</span>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
