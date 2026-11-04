import { Bar } from '@visx/shape';
import { scaleLinear } from '@visx/scale';

import { useMeasuredWidth } from './useMeasuredWidth';

// The drawing space, in CSS pixels: the svg is drawn at its container's measured width (360 before
// the first measurement). A bar is at most 6 wide in a 10 slot: geometry inside the drawn mark
// (guidelines § Scales).
const FALLBACK_W = 360;
const H = 40;
const AXIS_Y = 54;
const TOTAL_H = 58;
const BAR = 6;
const SLOT = 10;
const MIN_BAR = 2;
const MIB_PER_GIB = 1024;

export interface RunStripProps {
  /** The peaks of the last runs in MiB, oldest first. */
  peaks: number[];
  /** The cap in force, in MiB; a run over it is red. */
  capMib: number;
  /** Per run: true where the peak was cut short (the run was killed), so it is red whatever its value. */
  censored?: boolean[];
  /** The peak the full height stands for, in MiB. Default: the highest of the peaks and the cap. */
  max?: number;
  /** The label at the right end of the axis: when the newest run finished ("6m ago"). The left end always reads "oldest". Omitted, the right end is blank. */
  endLabel?: string;
}

const gib = (mib: number) => (mib / MIB_PER_GIB).toFixed(1);

/**
 * The last runs of a class as bars, oldest first: red where a run went over the cap or was cut
 * short, amber otherwise, with "oldest" at the left of the axis and a caller's label at the right.
 * The drawing is hidden from assistive tech; a summary is built from the data.
 *
 * @useWhen a class detail shows whether recent runs stayed under the cap (Memory's class panel).
 * For the spread of all peaks use PeakHistogram.
 * @states data, no runs, over the cap, censored, with an end label
 */
export function RunStrip({ peaks, capMib, censored = [], max, endLabel }: RunStripProps) {
  const [box, W] = useMeasuredWidth<HTMLDivElement>(FALLBACK_W);
  const top = max ?? Math.max(capMib, ...peaks);
  const y = scaleLinear<number>({ domain: [0, top], range: [0, H] });
  const slot = Math.min(SLOT, W / Math.max(peaks.length, 1));
  const bar = Math.min(BAR, slot * 0.6);
  const bad = peaks.map((p, i) => p > capMib || censored[i] === true);
  const over = bad.filter(Boolean).length;

  const label = peaks.length
    ? `Last ${peaks.length} runs, ${over} over the cap of ${gib(capMib)} GiB or cut short, newest ${gib(peaks[peaks.length - 1])} GiB`
    : 'No runs yet';

  return (
    <div ref={box} role="img" aria-label={label} className="w-full">
      <svg aria-hidden width={W} height={peaks.length ? TOTAL_H : H} className="block">
        {peaks.map((p, i) => {
          const h = Math.max(MIN_BAR, Math.min(H, y(p)));
          return <Bar key={i} x={i * slot} y={H - h} width={bar} height={h} className={bad[i] ? 'fill-red' : 'fill-amber'} />;
        })}
        {peaks.length > 0 && (
          <>
            <text x={0} y={AXIS_Y} className="fill-faint text-caption">
              oldest
            </text>
            {endLabel && (
              <text x={W} y={AXIS_Y} textAnchor="end" className="fill-faint text-caption">
                {endLabel}
              </text>
            )}
          </>
        )}
      </svg>
    </div>
  );
}
