import { ChevronDown, type LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { Button, type ButtonVariant, type PressHandler } from './Button';
import { cx, type PassThrough, type Size } from './ladder';
import { Menu, MenuItem } from './Menu';

/** One entry of the ▾ menu. */
export interface SplitButtonItem {
  /** The action, as a verb phrase. */
  label: string;
  /** Runs when chosen; the menu then closes. */
  onSelect: () => void;
  /** `danger` for an action that stops or removes something. */
  tone?: 'default' | 'danger';
}

export interface SplitButtonProps extends PassThrough<HTMLDivElement> {
  /** The main half's visible label. */
  children: ReactNode;
  /** How loud the control is, as Button's: it colours both halves. */
  variant?: ButtonVariant;
  /** The ladder step of both halves; the ▾ half is the square of the same step. */
  size?: Size;
  /** A lucide icon drawn before the main label. */
  icon?: LucideIcon;
  /** Runs when the main half is activated, unless it is loading; it receives the event. */
  onClick?: PressHandler;
  /** The ▾ trigger's accessible name, e.g. "More pause options". */
  menuLabel: string;
  /** The less common siblings of the main action, shown in the ▾ menu. */
  items: SplitButtonItem[];
  /** Shows a spinner on the main half and ignores its presses; the ▾ half is disabled until it ends, so a contradicting sibling cannot start. */
  loading?: boolean;
}

// One outlined box, one divider: the halves drop their own border and the inner corners, and each
// outer corner takes the step's radius back.
const box: Record<Size, string> = {
  sm: 'rounded-sm *:first:rounded-l-sm *:last:rounded-r-sm',
  md: 'rounded-sm *:first:rounded-l-sm *:last:rounded-r-sm',
  lg: 'rounded-md *:first:rounded-l-md *:last:rounded-r-md',
};

/**
 * A surface action with a less common sibling: a labelled main button with a ▾ half attached,
 * which opens a menu of the siblings. Built from Button and Menu's items, in one outlined box with
 * a divider between the halves.
 *
 * @useWhen one action leads a surface and one or two related, rarer ones belong beside it (Pause,
 * and ▾ then Drain). For several equal choices use Menu; for unrelated actions use two Buttons.
 * @states default, hover, pressed, focus-visible, menu open, loading
 */
export function SplitButton({
  children,
  variant = 'secondary',
  size = 'lg',
  icon,
  onClick,
  menuLabel,
  items,
  loading = false,
  ...rest
}: SplitButtonProps) {
  return (
    <div
      {...rest}
      role="group"
      className={cx(
        'inline-flex border border-border-control divide-x divide-border-control',
        '*:rounded-none *:border-0',
        box[size],
      )}
    >
      <Button variant={variant} size={size} icon={icon} loading={loading} onClick={onClick}>
        {children}
      </Button>
      <Menu label={menuLabel} icon={ChevronDown} iconOnly variant={variant} size={size} align="end" disabled={loading}>
        {items.map((item) => (
          <MenuItem key={item.label} tone={item.tone} onClick={item.onSelect}>
            {item.label}
          </MenuItem>
        ))}
      </Menu>
    </div>
  );
}
