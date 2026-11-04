import type { ReactNode } from 'react';
import type { PassThrough } from './ladder';
import type { StatusTone } from './StatusDot';

const TONE: Record<StatusTone, string> = {
  green: 'bg-green-soft text-green',
  amber: 'bg-amber-soft text-amber',
  red: 'bg-red-soft text-red',
  neutral: 'bg-surface-2 text-muted',
  accent: 'bg-accent-soft text-accent-text',
};

export interface StatusBadgeProps extends PassThrough<HTMLSpanElement> {
  /** The label's tint: green ok, amber caution, red fault, neutral plain, accent in progress. */
  tone: StatusTone;
  /** The short label, in sentence case: "Delivered", "Timed out". */
  children: ReactNode;
}

/**
 * A run's result or a class's status as a short tinted label.
 *
 * @useWhen naming an outcome in a table row or list: delivered, timed out, refused, infra error,
 * retried, pinned, warned. Not for a state beside a name (StatusDot) and not clickable.
 * @states green, amber, red, neutral, accent, truncated
 */
export function StatusBadge({ tone, children, ...rest }: StatusBadgeProps) {
  return (
    <span
      {...rest}
      className={`inline-block max-w-full truncate rounded-xs px-2xs py-3xs text-caption font-semibold align-middle ${TONE[tone]}`}
    >
      {children}
    </span>
  );
}
