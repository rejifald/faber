import { Button as BaseButton } from '@base-ui/react/button';
import type { LucideIcon } from 'lucide-react';
import type { MouseEvent, ReactNode } from 'react';
import { cx, focusRing, iconSize, ladder, square, type PassThrough, type Size } from './ladder';
import { spinner } from './Spinner';

/** How loud a button is. */
export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'link' | 'row' | 'danger';

/** The label's colour role: `danger` reads red on a secondary, ghost or link button. */
export type ButtonTone = 'default' | 'danger';

/** Colours per variant; every value is an ADR-0037 role. */
export const variants: Record<ButtonVariant, string> = {
  primary: 'bg-accent text-accent-ink hover:brightness-105 active:brightness-95',
  secondary: 'bg-surface-2 text-fg border border-border-control hover:bg-border-soft active:bg-border',
  ghost: 'bg-transparent text-fg hover:bg-surface-2 active:bg-border-soft',
  // Text in the link tone, no fill or border, still the step's full height to tap.
  link: 'bg-transparent text-accent-text hover:underline underline-offset-2',
  // A whole row that is one button (a settings profile): `ghost`'s surface-2 hover is all but
  // the popover's own bg, so the row fills with the border role, which shows in both themes.
  row: 'bg-transparent text-fg hover:bg-border active:bg-border-soft',
  // A confirmed destructive action (Remove in a confirmation row): `bg-red text-bg` holds 4.5:1 in both themes.
  danger: 'bg-red text-bg hover:brightness-105 active:brightness-95',
};

/**
 * The parts of a button every kit button shares, sized from the ladder. A disabled button keeps
 * its pointer events, so its tooltip can still say why; Base UI refuses the activation.
 */
export function buttonClass(size: Size, variant: ButtonVariant, isSquare: boolean, tone: ButtonTone = 'default'): string {
  // `danger` tone turns the label red on the quiet variants; primary and danger already own their ink.
  const colour =
    tone === 'danger' && (variant === 'secondary' || variant === 'ghost' || variant === 'link')
      ? variants[variant].replace(/text-(fg|accent-text)/, 'text-red')
      : variants[variant];
  return cx(
    'inline-flex shrink-0 items-center font-semibold whitespace-nowrap select-none',
    'cursor-default transition-colors data-disabled:opacity-50',
    isSquare ? square[size] : ladder[size],
    colour,
    focusRing,
  );
}

/** What a kit button calls on activation; the event is passed on, since a wrapper such as Tooltip merges its own handler in and reads it. */
export type PressHandler = (event: MouseEvent<HTMLButtonElement>) => void;

/** The click handler of a control that may be loading: a press while loading does nothing. */
export function pressHandler(loading: boolean, onClick?: PressHandler): PressHandler {
  return (event) => {
    if (loading) {
      event.preventDefault();
      return;
    }
    onClick?.(event);
  };
}

export interface ButtonProps extends PassThrough<HTMLButtonElement> {
  /** The visible label. */
  children: ReactNode;
  /** The ladder step (ADR-0039 § 2): `sm` inline on a row, `md` in menus and forms, `lg` a surface's own actions. */
  size?: Size;
  /** How loud it is: `primary` for the one main action of a surface; `link` for secondary and rarely used actions; `secondary` (outlined, the default) for neutral tool buttons only; `ghost` in dense rows; `danger` for a confirmed destructive action. */
  variant?: ButtonVariant;
  /** `danger` makes the label red on a secondary, ghost or link button, for an action that starts a destructive flow; ignored on primary and danger. */
  tone?: ButtonTone;
  /** A lucide icon drawn before the label, sized by the step. */
  icon?: LucideIcon;
  /** Disables it, only while a sibling action in the same place runs; any other "can't now" stays live and says why on press. It stays focusable. */
  disabled?: boolean;
  /** Shows a spinner in place of the icon and ignores presses, keeping its label, look and focus. */
  loading?: boolean;
  /** `submit` inside a form; `button` otherwise. */
  type?: 'button' | 'submit';
  /** Runs on activation by pointer or keyboard, unless disabled or loading; it receives the event. */
  onClick?: PressHandler;
}

/**
 * A labelled button on the control ladder, wrapping Base UI's Button.
 *
 * @useWhen any action with a visible label. `primary` for a surface's one main action; `link`
 * for secondary and rarely used actions; the outlined `secondary` for neutral tool buttons only;
 * `ghost` inside dense rows; `row` for a whole row that is one button; `danger` for a confirmed destructive action, such as Remove in a confirmation row; `tone="danger"` on a secondary for an action that starts a destructive flow, such as Remove before its confirmation. For an icon alone use IconButton; for a choice of options use Menu or
 * Select; never a raw `<button>` (ADR-0042 § 2).
 * @states default, hover, pressed, focus-visible, disabled, loading
 */
export function Button({
  children,
  size = 'md',
  variant = 'secondary',
  tone = 'default',
  icon: Icon,
  disabled = false,
  loading = false,
  type = 'button',
  onClick,
  ...rest
}: ButtonProps) {
  return (
    <BaseButton
      {...rest}
      type={type}
      disabled={disabled}
      focusableWhenDisabled
      aria-busy={loading || undefined}
      onClick={pressHandler(loading, onClick)}
      className={buttonClass(size, variant, false, tone)}
    >
      {loading ? spinner(size) : Icon && <Icon aria-hidden className={iconSize[size]} />}
      {children}
    </BaseButton>
  );
}
