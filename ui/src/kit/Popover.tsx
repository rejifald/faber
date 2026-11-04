import { Popover as BasePopover } from '@base-ui/react/popover';
import type { ReactNode } from 'react';
import { buttonClass, type ButtonVariant } from './Button';
import { cx, type Size } from './ladder';

export interface PopoverProps {
  /** The trigger's accessible name, only when it draws no text (an icon). A trigger with visible
   * text is named by that text, so a screen reader hears what is on screen (WCAG 2.5.3). */
  label?: string;
  /** What the trigger draws: text, or a whole row's content. */
  trigger: ReactNode;
  /** How loud the trigger is, as Button's; default `ghost`, `row` for a trigger that is a whole row. */
  triggerVariant?: ButtonVariant;
  /** The trigger's ladder step. */
  size?: Size;
  /** The popup's heading ("Other hosts"); it also names the popup for a screen reader. */
  title?: string;
  /** Which way the popup opens from the trigger. */
  align?: 'start' | 'center' | 'end';
  /** The read-only content of the popup. */
  children: ReactNode;
}

/**
 * A button that opens a small panel of read-only content, wrapping Base UI's Popover. Focus moves
 * into the popup, Escape or a press outside closes it and focus returns to the trigger.
 *
 * @useWhen read-only content shown from a trigger (HostCard's list of other hosts). Not for
 * actions (use Menu) and not for a hint on hover (use Tooltip).
 * @states closed, hover, focus-visible, open
 */
export function Popover({
  label,
  trigger,
  triggerVariant = 'ghost',
  size = 'md',
  title,
  align = 'center',
  children,
}: PopoverProps) {
  return (
    <BasePopover.Root>
      <BasePopover.Trigger
        aria-label={label}
        className={cx(buttonClass(size, triggerVariant, false), triggerVariant === 'row' && 'w-full')}
      >
        {trigger}
      </BasePopover.Trigger>
      <BasePopover.Portal>
        <BasePopover.Positioner align={align} sideOffset={4}>
          <BasePopover.Popup className="flex flex-col rounded-md border border-border bg-elevate p-2xs shadow-floating outline-hidden">
            {title && <BasePopover.Title className="px-sm py-2xs text-caption font-semibold text-muted">{title}</BasePopover.Title>}
            {children}
          </BasePopover.Popup>
        </BasePopover.Positioner>
      </BasePopover.Portal>
    </BasePopover.Root>
  );
}
