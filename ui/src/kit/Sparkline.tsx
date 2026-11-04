import { LinePath } from '@visx/shape';
import { scaleLinear } from '@visx/scale';

/** The colour role of a sparkline's line. */
export type SparklineTone = 'neutral' | 'green' | 'amber' | 'red' | 'accent';

// The drawing space, in SVG user units, matching the theme's `w-spark h-spark` (52 x 20 px) so the
// stroke is 1.7 px. The stroke, the dot and the inset that keeps them inside the box are geometry
// inside the drawn mark (guidelines § Scales).
const W = 52;
const H = 20;
const STROKE = 1.7;
const DOT = 2;
const INSET = 2.5;

const stroke: Record<SparklineTone, string> = {
  neutral: 'stroke-muted',
  green: 'stroke-green',
  amber: 'stroke-amber',
  red: 'stroke-red',
  accent: 'stroke-accent',
};
const fill: Record<SparklineTone, string> = {
  neutral: 'fill-muted',
  green: 'fill-green',
  amber: 'fill-amber',
  red: 'fill-red',
  accent: 'fill-accent',
};

export interface SparklineProps {
  /** The series, oldest first. Evenly spaced; the line fills the box from its minimum to its maximum. */
  values: number[];
  /** The line's colour role. Default `neutral`. */
  tone?: SparklineTone;
  /** The text summary, read by assistive tech: what it measures, over what, and the trend ("Delivered rate over 24 hours, rising"). */
  label: string;
}

/**
 * The trend behind one number, inside a stat tile: a small line with no axes and no tooltip. It
 * scales to its own minimum and maximum, so it shows shape, not magnitude; the figure beside it
 * carries the value.
 *
 * @useWhen a stat tile shows a number whose recent history matters (dashboard/anatomy.md § 5, Sparkline).
 * @states data, one point, empty, flat
 */
export function Sparkline({ values, tone = 'neutral', label }: SparklineProps) {
  const n = values.length;
  const lo = n ? Math.min(...values) : 0;
  const hi = n ? Math.max(...values) : 0;
  const x = scaleLinear<number>({ domain: [0, Math.max(n - 1, 1)], range: [INSET, W - INSET] });
  // A flat series has no range to scale: it sits on the middle line.
  const y = scaleLinear<number>({ domain: lo === hi ? [lo - 1, hi + 1] : [lo, hi], range: [H - INSET, INSET] });

  return (
    <svg role="img" aria-label={label} viewBox={`0 0 ${W} ${H}`} className="block h-spark w-spark shrink-0">
      {n === 1 && <circle cx={W / 2} cy={H / 2} r={DOT} className={fill[tone]} />}
      {n > 1 && (
        <LinePath
          data={values}
          x={(_, i) => x(i)}
          y={(v) => y(v)}
          fill="none"
          strokeWidth={STROKE}
          strokeLinecap="round"
          strokeLinejoin="round"
          className={stroke[tone]}
        />
      )}
    </svg>
  );
}
