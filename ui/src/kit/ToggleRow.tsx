import { Toggle } from '@base-ui/react/toggle';
import type { ReactNode } from 'react';
import { buttonClass } from './Button';
import { cx, type Size } from './ladder';

export interface ToggleRowProps {
  /** Whether the row is on. */
  pressed: boolean;
  /** Called with the new state when the row is pressed. */
  onPressedChange: (pressed: boolean) => void;
  /** The row's content: a label, and optionally a dot before it and a count after it. */
  children: ReactNode;
  /** The ladder step; default `md`. */
  size?: Size;
  /** The label's weight: `semibold` as every kit button, or `regular` for a long list of values read as plain text (Activity's facets). Default `semibold`. */
  weight?: 'regular' | 'semibold';
}

/**
 * A whole row that switches on and off, wrapping Base UI's Toggle: a filter value in a list of
 * several that may all be on at once. On, it rests on the accent wash in accent text, as the
 * dashboard nav's selected row does; children that colour themselves should follow the
 * `data-pressed` state (a count turns `text-accent-text`, since faint fails on the wash).
 *
 * @useWhen one of many independent filter values in a column (Activity's facets). For one choice
 * among a few use SegmentedControl; for a setting use Switch.
 * @states off, hover, on, focus-visible, regular weight
 */
export function ToggleRow({ pressed, onPressedChange, children, size = 'md', weight = 'semibold' }: ToggleRowProps) {
  return (
    <Toggle
      pressed={pressed}
      onPressedChange={(next) => onPressedChange(next)}
      className={cx(
        buttonClass(size, 'row', false),
        'group/toggle w-full data-pressed:bg-accent-soft data-pressed:text-accent-text',
        weight === 'regular' && 'font-normal!',
      )}
    >
      {children}
    </Toggle>
  );
}
