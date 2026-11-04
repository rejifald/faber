import type { PassThrough } from './ladder';

/** The tones a dot or badge can carry. */
export type StatusTone = 'green' | 'amber' | 'red' | 'neutral' | 'accent';

const FILL: Record<StatusTone, string> = {
  green: 'fill-green',
  amber: 'fill-amber',
  red: 'fill-red',
  neutral: 'fill-faint',
  accent: 'fill-accent',
};

const STROKE: Record<StatusTone, string> = {
  green: 'stroke-green',
  amber: 'stroke-amber',
  red: 'stroke-red',
  neutral: 'stroke-faint',
  accent: 'stroke-accent',
};

export interface StatusDotProps extends PassThrough<SVGSVGElement & HTMLElement> {
  /** The state's colour: green ok, amber caution, red fault, neutral idle, accent running. */
  tone: StatusTone;
  /** A ring pulses outward from the dot (running). Still when the reader prefers reduced motion. */
  pulse?: boolean;
  /** A hollow dot for a state in transition (booting, stopping). It wins over `pulse`. */
  ring?: boolean;
  /** Names the dot when it is the only carrier of the state; without it the dot is hidden from assistive tech. */
  label?: string;
}

/**
 * A small coloured dot that carries a state beside the words that say it.
 *
 * @useWhen marking a host, runner or job with its state next to a text label. Not alone, unless
 * `label` names it, and not for a result word: use StatusBadge.
 * @states green, amber, red, neutral, accent, pulse, ring
 */
export function StatusDot({ tone, pulse = false, ring = false, label, ...rest }: StatusDotProps) {
  const fill = FILL[tone];
  return (
    <svg
      {...(rest as object)}
      viewBox="0 0 8 8"
      className="size-sm shrink-0"
      role={label ? 'img' : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
    >
      {pulse && !ring && (
        <circle
          cx="4"
          cy="4"
          r="4"
          className={`${fill} origin-center motion-safe:animate-pulse-ring motion-reduce:hidden`}
          style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
        />
      )}
      {ring ? (
        <circle cx="4" cy="4" r="2.25" strokeWidth="1.5" fill="none" className={STROKE[tone]} />
      ) : (
        <circle cx="4" cy="4" r="2" className={fill} />
      )}
    </svg>
  );
}
