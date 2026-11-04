import { Toggle } from '@base-ui/react/toggle';
import { ToggleGroup } from '@base-ui/react/toggle-group';
import type { ReactNode } from 'react';
import { cx, focusRing, ladder, type Size } from './ladder';

/** One choice: its value, its label and, optionally, how many items it holds. */
export interface SegmentedItem {
  value: string;
  label: ReactNode;
  /** A count shown after the label, in the monospace face. */
  count?: number;
  /** Colours the label and count for a choice that needs attention. */
  tone?: 'amber' | 'red';
}

export interface SegmentedControlProps {
  /** The choices, in order (2 to 6). */
  items: SegmentedItem[];
  /** The selected choice's value. One is always selected. */
  value: string;
  /** Called with the newly selected choice's value. */
  onValueChange: (value: string) => void;
  /** Names the group for assistive tech, e.g. "Filter activity". */
  label: string;
  /** The ladder step. Defaults to `md`. */
  size?: Size;
}

const TONE = { amber: 'text-amber', red: 'text-red' } as const;

/**
 * A track of two to six peer choices over the same content, exactly one always selected.
 *
 * @useWhen filtering one list or table by a category (All, Delivered, Faults). Not Tabs, since
 * nothing here owns a panel, and not for a free multi-select.
 * @states default, hover, selected, focus-visible, with counts, toned item
 */
export function SegmentedControl({ items, value, onValueChange, label, size = 'md' }: SegmentedControlProps) {
  return (
    <ToggleGroup
      value={[value]}
      onValueChange={(next) => {
        // Pressing the selected item would empty the group; the choice never goes empty.
        const chosen = next[0];
        if (chosen !== undefined) onValueChange(String(chosen));
      }}
      aria-label={label}
      className="flex gap-3xs self-start rounded-md bg-surface-2 p-3xs"
    >
      {items.map((item) => (
        <Toggle
          key={item.value}
          value={item.value}
          className={cx(
            'inline-flex items-center font-semibold select-none cursor-default',
            ladder[size],
            item.tone ? TONE[item.tone] : 'text-muted hover:text-fg data-pressed:text-fg-strong',
            'data-pressed:bg-surface data-pressed:shadow-raised',
            focusRing,
          )}
        >
          {item.label}
          {item.count !== undefined && (
            <>
              <span className="sr-only">, </span>
              <span className={cx('font-mono', !item.tone && 'text-faint')}>{item.count}</span>
            </>
          )}
        </Toggle>
      ))}
    </ToggleGroup>
  );
}
