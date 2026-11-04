import { Bar } from '@visx/shape';
import { scaleLinear } from '@visx/scale';

import { useMeasuredWidth } from './useMeasuredWidth';

// The drawing space, in CSS pixels: the svg is drawn at its container's measured width (W below
// is that width, 360 before the first measurement), so its labels stay at the caption size.
// Row heights, label offsets and gaps are geometry inside the drawn mark (guidelines § Scales).
const FALLBACK_W = 360;
const H = 124;
const TOP = 30;
const BASE = 104;
const GAP = 2;
const LABEL_Y = 10;
const COUNT_LIFT = 4;
const AXIS_Y = 120;
const MIB_PER_GIB = 1024;

const gib = (mib: number) => mib / MIB_PER_GIB;

export interface PeakHistogramProps {
  /** The class's measured peaks in MiB, one per run. */
  peaks: number[];
  /** The cap in force, in MiB; drawn as a labelled line. */
  capMib: number;
  /** The cold-safe peak, in MiB, drawn as a second labelled line; omitted when the class has none. */
  safeMib?: number;
  /** The width of one bucket in MiB. Default 1024 (1 GiB). */
  bucketMib?: number;
  /** Where the cold cluster starts, in MiB, when the class splits into a warm and a cold one: buckets from there up are red, the warm ones amber. Omitted when there is no split, and every bucket is amber. */
  coldFromMib?: number;
}

/** Peaks grouped into buckets of `size`, from `lo` upward; `counts[i]` covers `[lo + i * size, lo + (i + 1) * size)`. */
function bucketise(peaks: number[], size: number, lo: number, n: number): number[] {
  const counts = new Array<number>(n).fill(0);
  for (const p of peaks) counts[Math.min(n - 1, Math.floor((p - lo) / size))] += 1;
  return counts;
}

function median(values: number[]): number {
  const s = [...values].sort((a, b) => a - b);
  const mid = s.length >> 1;
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

/** "47 samples, 38 near 3.1 GiB, 9 near 8.2 GiB": each run of filled buckets is one cluster, named by its median. */
function summary(peaks: number[], counts: number[], size: number, lo: number, capMib: number, safeMib?: number): string {
  if (!peaks.length) return 'No samples yet';
  const clusters: string[] = [];
  let start = -1;
  for (let i = 0; i <= counts.length; i += 1) {
    const filled = i < counts.length && counts[i] > 0;
    if (filled && start < 0) start = i;
    if (!filled && start >= 0) {
      const from = lo + start * size;
      const to = lo + i * size;
      const inside = peaks.filter((p) => p >= from && (p < to || i === counts.length));
      clusters.push(`${inside.length} near ${gib(median(inside)).toFixed(1)} GiB`);
      start = -1;
    }
  }
  const lines = [`cap ${gib(capMib).toFixed(1)} GiB`];
  if (safeMib !== undefined) lines.push(`cold-safe ${gib(safeMib).toFixed(1)} GiB`);
  return `${peaks.length} samples, ${clusters.join(', ')}. ${lines.join(', ')}.`;
}

/**
 * A class's measured peaks in buckets, with the cap and the cold-safe value as marked lines and
 * each bucket's count above its bar. The warm cluster is amber and, when the class is split, the
 * cold cluster red; with no split every bucket is amber. A bucket wholly over the cap has a red
 * outline. The drawing is hidden from assistive tech; an accessible summary is built from the data.
 *
 * @useWhen a class detail shows how its peaks are spread, to explain a cap or a bimodal class
 * (Memory's class panel). For a plain trend use Sparkline.
 * @states data, no samples, one bucket, with a cold-safe line, with a cold cluster, over the cap
 */
export function PeakHistogram({ peaks, capMib, safeMib, bucketMib = MIB_PER_GIB, coldFromMib }: PeakHistogramProps) {
  const [box, W] = useMeasuredWidth<HTMLDivElement>(FALLBACK_W);
  const marks = [capMib, ...(safeMib === undefined ? [] : [safeMib])];
  const all = [...peaks, ...marks];
  const lo = Math.floor(Math.min(...all) / bucketMib) * bucketMib;
  const hi = (Math.floor(Math.max(...all) / bucketMib) + 1) * bucketMib;
  const n = Math.max(1, Math.round((hi - lo) / bucketMib));
  const counts = bucketise(peaks, bucketMib, lo, n);
  const top = Math.max(1, ...counts);

  const x = scaleLinear<number>({ domain: [lo, hi], range: [0, W] });
  const y = scaleLinear<number>({ domain: [0, top], range: [BASE, TOP] });
  const slot = x(lo + bucketMib) - x(lo);

  // The longest empty stretch between two filled buckets is named, so a gap reads as "no runs".
  let gapAt = -1;
  let gapLen = 0;
  for (let i = 0, run = 0; i < n; i += 1) {
    run = counts[i] === 0 ? run + 1 : 0;
    const next = counts.slice(i + 1).some((c) => c > 0);
    const before = counts.slice(0, i - run + 1).some((c) => c > 0);
    if (run > gapLen && run >= 2 && next && before) {
      gapLen = run;
      gapAt = i - run + 1;
    }
  }

  const label = summary(peaks, counts, bucketMib, lo, capMib, safeMib);
  const lines = [
    { mib: capMib, text: `cap ${gib(capMib).toFixed(1)}`, stroke: 'stroke-muted', fill: 'fill-muted' },
    ...(safeMib === undefined
      ? []
      : [{ mib: safeMib, text: `safe ${gib(safeMib).toFixed(1)}`, stroke: 'stroke-amber', fill: 'fill-amber' }]),
  ];

  return (
    <div ref={box} role="img" aria-label={label} className="w-full">
      <svg aria-hidden width={W} height={H} className="block">
        {counts.map((c, i) => {
          const from = lo + i * bucketMib;
          const h = c ? BASE - y(c) : 0;
          return (
            c > 0 && (
              <g key={i}>
                <Bar
                  x={x(from) + GAP / 2}
                  y={BASE - h}
                  width={Math.max(slot - GAP, 1)}
                  height={h}
                  className={`${coldFromMib !== undefined && from + bucketMib > coldFromMib ? 'fill-red' : 'fill-amber'} ${from >= capMib ? 'stroke-red' : 'stroke-transparent'}`}
                  strokeWidth={1.5}
                />
                <text
                  x={x(from) + slot / 2}
                  y={BASE - h - COUNT_LIFT}
                  textAnchor="middle"
                  className="fill-fg font-mono text-caption tabular-nums"
                >
                  {c}
                </text>
              </g>
            )
          );
        })}
        <line x1={0} x2={W} y1={BASE} y2={BASE} className="stroke-border" />
        {lines.map((l) => {
          const at = x(l.mib);
          const flip = at > W / 2;
          return (
            <g key={l.text}>
              <line x1={at} x2={at} y1={LABEL_Y + 2} y2={BASE} strokeWidth={1} className={l.stroke} />
              <text
                x={at + (flip ? -3 : 3)}
                y={LABEL_Y}
                textAnchor={flip ? 'end' : 'start'}
                className={`${l.fill} font-mono text-caption tabular-nums`}
              >
                {l.text}
              </text>
            </g>
          );
        })}
        <text x={0} y={AXIS_Y} className="fill-faint font-mono text-caption tabular-nums">
          {gib(lo).toFixed(0)}G
        </text>
        {gapAt >= 0 && (
          <text x={x(lo + (gapAt + gapLen / 2) * bucketMib)} y={AXIS_Y} textAnchor="middle" className="fill-faint font-mono text-caption tabular-nums">
            no runs {gib(lo + gapAt * bucketMib).toFixed(0)}–{gib(lo + (gapAt + gapLen) * bucketMib).toFixed(0)}G
          </text>
        )}
        <text x={W} y={AXIS_Y} textAnchor="end" className="fill-faint font-mono text-caption tabular-nums">
          {gib(hi).toFixed(0)}G
        </text>
      </svg>
    </div>
  );
}
