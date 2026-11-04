import { Button as BaseButton } from '@base-ui/react/button';
import { Minus, Plus } from 'lucide-react';
import type { KeyboardEvent } from 'react';
import { cx, focusRing, iconSize, type Size } from './ladder';

/** `value` moved by `delta`, held to `min` and `max` when set, and rounded so 0.05 steps do not drift. */
export function stepValue(value: number, delta: number, min?: number, max?: number): number {
  let next = Math.round((value + delta) * 1e6) / 1e6;
  if (min !== undefined) next = Math.max(min, next);
  if (max !== undefined) next = Math.min(max, next);
  return next;
}

export interface StepperProps {
  /** The number it holds. */
  value: number;
  /** What shows for it, set by the caller ("2.0 GiB", "1.2×", "Off"). */
  display: string;
  /** Its accessible name ("Reserve"). */
  label: string;
  /** How much one press changes the value. */
  step: number;
  /** The lowest value; the decrement stays live there and does nothing. */
  min?: number;
  /** The highest value; the increment stays live there and does nothing. */
  max?: number;
  /** Called with the next value, held to min and max, on every press or key. */
  onChange: (next: number) => void;
  /** The border colour: `success` after a change applied, `warning` for a value that needs a word. */
  tone?: 'default' | 'success' | 'warning';
  /** Dims the value while a change applies; presses still count. */
  busy?: boolean;
  /** The ladder step. */
  size?: Size;
  /** The decrement button's name. */
  decrementLabel?: string;
  /** The increment button's name. */
  incrementLabel?: string;
}

const TONE = { default: 'border-border-control', success: 'border-green', warning: 'border-amber' } as const;

const BOX: Record<Size, string> = {
  sm: 'min-h-control-sm rounded-sm',
  md: 'min-h-control-md rounded-sm',
  lg: 'min-h-control-lg rounded-md',
};
const WIDE: Record<Size, string> = { sm: 'min-w-control-sm', md: 'min-w-control-md', lg: 'min-w-control-lg' };
const TEXT: Record<Size, string> = { sm: 'text-caption', md: 'text-small', lg: 'text-body' };

const half = 'inline-flex shrink-0 items-center justify-center self-stretch text-muted cursor-default select-none transition-colors hover:bg-surface-2 active:bg-border-soft';

/**
 * A number changed by fixed steps, shown as a formatted string, one bordered box with the
 * decrement at the left, the value centred in mono and the increment at the right. The value is
 * a spinbutton: it takes focus, and the arrows, Page keys, Home and End change it.
 *
 * @useWhen a value that applies as you press, where typing is not needed. For a free number or
 * text use Field; for a choice of named options use Select.
 * @states default, hover, pressed, focus-visible, at min or max (stays live), busy, success, warning
 */
export function Stepper({
  value,
  display,
  label,
  step,
  min,
  max,
  onChange,
  tone = 'default',
  busy = false,
  size = 'md',
  decrementLabel = 'Less',
  incrementLabel = 'More',
}: StepperProps) {
  const by = (delta: number) => onChange(stepValue(value, delta, min, max));
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const moves: Record<string, () => void> = {
      ArrowUp: () => by(step),
      ArrowDown: () => by(-step),
      PageUp: () => by(step * 5),
      PageDown: () => by(-step * 5),
      ...(min !== undefined && { Home: () => onChange(min) }),
      ...(max !== undefined && { End: () => onChange(max) }),
    };
    const move = moves[e.key];
    if (!move) return;
    e.preventDefault();
    move();
  };
  return (
    <div
      role="group"
      aria-label={label}
      className={cx('inline-flex items-stretch border bg-surface transition-colors', BOX[size], TONE[tone])}
    >
      <BaseButton type="button" tabIndex={-1} aria-hidden aria-label={decrementLabel} onClick={() => by(-step)} className={cx(half, WIDE[size], 'rounded-l-sm')}>
        <Minus aria-hidden className={iconSize[size]} />
      </BaseButton>
      <div
        role="spinbutton"
        tabIndex={0}
        aria-label={label}
        aria-valuenow={value}
        aria-valuetext={display}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-busy={busy || undefined}
        onKeyDown={onKeyDown}
        className={cx(
          'grid place-items-center px-sm text-center font-mono font-medium text-fg-strong transition-opacity',
          TEXT[size],
          busy && 'opacity-50',
          focusRing,
        )}
      >
        {/* The widest realistic value ("12.5 GiB") sets the minimum width, so the box does not jump. */}
        <span aria-hidden className="invisible col-start-1 row-start-1">
          12.5 GiB
        </span>
        <span className="col-start-1 row-start-1">{display}</span>
      </div>
      <BaseButton type="button" tabIndex={-1} aria-hidden aria-label={incrementLabel} onClick={() => by(step)} className={cx(half, WIDE[size], 'rounded-r-sm')}>
        <Plus aria-hidden className={iconSize[size]} />
      </BaseButton>
    </div>
  );
}
