import type { ReactNode } from 'react';
import { cx, focusRingInset } from './ladder';

export interface RowButtonProps {
  /** What the row says: text and small marks, on one line. */
  children: ReactNode;
  /** Called when the row is pressed. */
  onClick: () => void;
  /** The row's accessible name, when its marks say more than its text (`Show 11 more: 3 running`). */
  label?: string;
  /** Set when the row opens or closes something: whether that is open now. */
  expanded?: boolean;
  /** The id of what `expanded` opens. */
  controls?: string;
  /** Takes the room left in its flex row, so a sibling sits at the end. */
  grow?: boolean;
}

/**
 * A full-bleed one-line row that does something when pressed, on the grid of the list lines
 * around it: `small` muted text, no fill, no radius and no accent, a neutral lift on hover
 * (`row-hover`) and a stronger one while pressed (`row-press`), the focus ring drawn inside it.
 * Its marks keep their own colours on every state.
 *
 * @useWhen the line that ends a folded list ("Show 11 more", "Open panel"). For a row that opens
 * detail under itself use Disclosure; for a button inside a card use Button.
 * @states default, hover, pressed, focus-visible
 */
export function RowButton({ children, onClick, label, expanded, controls, grow = false }: RowButtonProps) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-expanded={expanded}
      aria-controls={controls}
      onClick={onClick}
      className={cx(
        'group/row flex min-w-0 items-center gap-sm px-xl py-3xs text-left text-small whitespace-nowrap text-muted',
        'cursor-default select-none hover:bg-row-hover active:bg-row-press',
        grow && 'flex-1',
        focusRingInset,
      )}
    >
      {children}
    </button>
  );
}
