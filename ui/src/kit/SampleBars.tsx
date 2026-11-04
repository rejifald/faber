// The drawing space, in SVG user units, matching the theme's `w-ticks h-ticks` (14 x 10 px) so one
// unit is one pixel. Four ticks, each 2 wide with a 2 gap, rising in height: geometry inside the
// drawn mark (guidelines § Scales).
const W = 14;
const H = 10;
const TICK = 2;
const STEP = 4;
const HEIGHTS = [4, 6, 8, 10];

export interface SampleBarsProps {
  /** How many samples the class has. */
  count: number;
  /** The count at which learning is done; below it the ticks are amber. Default 15. */
  threshold?: number;
}

/**
 * How full a class's sample window is, as four rising ticks beside the count. Under the learning
 * threshold at most three ticks are filled and they read amber; at the threshold all four fill.
 * Drawn only for the eye: the count beside it is the text.
 *
 * @useWhen a class row shows its sample count and how far that is from trusted (Memory's classes table).
 * @states empty, learning, learned
 */
export function SampleBars({ count, threshold = 15 }: SampleBarsProps) {
  const learning = count < threshold;
  const filled = count <= 0 ? 0 : learning ? Math.max(1, Math.ceil((count / threshold) * 3)) : HEIGHTS.length;
  return (
    <svg aria-hidden viewBox={`0 0 ${W} ${H}`} className="block h-ticks w-ticks shrink-0">
      {HEIGHTS.map((h, i) => (
        <rect
          key={i}
          x={i * STEP}
          y={H - h}
          width={TICK}
          height={h}
          className={i < filled ? (learning ? 'fill-amber' : 'fill-fg') : 'fill-border'}
        />
      ))}
    </svg>
  );
}
