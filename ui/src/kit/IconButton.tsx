import { Button as BaseButton } from '@base-ui/react/button';
import type { LucideIcon } from 'lucide-react';
import { buttonClass, pressHandler, type ButtonVariant, type PressHandler } from './Button';
import { iconSize, type PassThrough, type Size } from './ladder';
import { spinner } from './Spinner';
import { Tooltip } from './Tooltip';

export interface IconButtonProps extends PassThrough<HTMLButtonElement> {
  /** The lucide icon, sized by the step. */
  icon: LucideIcon;
  /** What it does, in words: its accessible name and its tooltip. Required, an icon is not a label. */
  label: string;
  /** The ladder step; the button is the square of that step (ADR-0039 § 1). */
  size?: Size;
  /** How loud it is; `ghost` by default, since icon buttons sit in rows and headers. */
  variant?: ButtonVariant;
  /** Disables it, only while a sibling action in the same place runs; any other "can't now" stays live and says why on press. Its tooltip still shows. */
  disabled?: boolean;
  /** Shows a spinner in place of the icon and ignores presses, keeping its look and focus. */
  loading?: boolean;
  /** Runs on activation by pointer or keyboard, unless disabled or loading; it receives the event. */
  onClick?: PressHandler;
}

/**
 * An icon-only square button with its label as a tooltip, wrapping Base UI's Button.
 *
 * @useWhen a well-known action in a tight spot: the ⋯ menu trigger, Reconcile, a row's remove.
 * If the action needs its word to be understood, use Button with an icon instead.
 * @states default, hover, pressed, focus-visible, disabled, loading
 */
export function IconButton({
  icon: Icon,
  label,
  size = 'md',
  variant = 'ghost',
  disabled = false,
  loading = false,
  onClick,
  ...rest
}: IconButtonProps) {
  return (
    <Tooltip content={label}>
      <BaseButton
        {...rest}
        type="button"
        aria-label={label}
        aria-busy={loading || undefined}
        disabled={disabled}
        focusableWhenDisabled
        onClick={pressHandler(loading, onClick)}
        className={buttonClass(size, variant, true)}
      >
        {loading ? spinner(size) : <Icon aria-hidden className={iconSize[size]} />}
      </BaseButton>
    </Tooltip>
  );
}
