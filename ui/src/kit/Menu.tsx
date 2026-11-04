import { Menu as BaseMenu } from '@base-ui/react/menu';
import { Tooltip as BaseTooltip } from '@base-ui/react/tooltip';
import { Check, type LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { buttonClass, type ButtonVariant } from './Button';
import { cx, focusRing, iconSize, ladder, type Size } from './ladder';
import { tooltipPopup } from './Tooltip';

export interface MenuProps {
  /** The trigger's label; with `iconOnly`, its accessible name and tooltip instead. */
  label: string;
  /** A lucide icon on the trigger. */
  icon?: LucideIcon;
  /** Draws the trigger as the square of its step, showing only `icon` (the ⋯ button). */
  iconOnly?: boolean;
  /** The trigger's ladder step. Items are always `md` (ADR-0039 § 2). */
  size?: Size;
  /** How loud the trigger is, as Button's; default `ghost`. */
  variant?: ButtonVariant;
  /** Which way the popup opens from the trigger. */
  align?: 'start' | 'center' | 'end';
  /** Disables the trigger, as while a sibling action runs (SplitButton's ▾ half). */
  disabled?: boolean;
  /** Whether the popup shows, when the screen must close it itself (the popover closes it on blur). Omit to let the menu keep its own state. */
  open?: boolean;
  /** Called when the menu opens or closes, by its trigger, a choice, Escape or a press outside. */
  onOpenChange?: (open: boolean) => void;
  /** The items: MenuItem, MenuCheckboxItem and MenuSeparator. */
  children: ReactNode;
}

/**
 * A button that opens a list of actions, wrapping Base UI's Menu.
 *
 * @useWhen a handful of secondary actions that would crowd the surface (the popover's ⋯).
 * For picking a value use Select; for one or two actions show Buttons.
 * @states closed, open, item highlighted, item disabled
 */
export function Menu({
  label,
  icon: Icon,
  iconOnly = false,
  size = 'md',
  variant = 'ghost',
  align = 'end',
  disabled = false,
  open,
  onOpenChange,
  children,
}: MenuProps) {
  const trigger = (
    <BaseMenu.Trigger
      disabled={disabled}
      aria-label={iconOnly ? label : undefined}
      className={buttonClass(size, variant, iconOnly)}
    >
      {Icon && <Icon aria-hidden className={iconSize[size]} />}
      {!iconOnly && label}
    </BaseMenu.Trigger>
  );
  return (
    <BaseMenu.Root open={open} onOpenChange={onOpenChange ? (next) => onOpenChange(next) : undefined}>
      {iconOnly ? (
        <BaseTooltip.Root>
          <BaseTooltip.Trigger render={trigger} />
          <BaseTooltip.Portal>
            <BaseTooltip.Positioner sideOffset={6}>
              <BaseTooltip.Popup className={tooltipPopup}>
                {label}
              </BaseTooltip.Popup>
            </BaseTooltip.Positioner>
          </BaseTooltip.Portal>
        </BaseTooltip.Root>
      ) : (
        trigger
      )}
      <BaseMenu.Portal>
        <BaseMenu.Positioner align={align} sideOffset={4}>
          <BaseMenu.Popup className="flex flex-col rounded-md border border-border bg-elevate p-2xs shadow-floating outline-hidden">
            {children}
          </BaseMenu.Popup>
        </BaseMenu.Positioner>
      </BaseMenu.Portal>
    </BaseMenu.Root>
  );
}

export interface MenuItemProps {
  /** The action, as a verb phrase. */
  children: ReactNode;
  /** A lucide icon before the label. */
  icon?: LucideIcon;
  /** `danger` for an action that stops or removes something. */
  tone?: 'default' | 'danger';
  /** The keyboard shortcut that does the same with the menu shut, as macOS spells it (`⌘Q`). */
  shortcut?: string;
  /** Shows it but refuses activation, only while a sibling action in the same place runs. */
  disabled?: boolean;
  /** Keeps the menu open after it is chosen, so the item can show what it changed. */
  keepOpen?: boolean;
  /** Runs when chosen by pointer or keyboard; the menu then closes unless `keepOpen`. */
  onClick?: () => void;
}

/** A menu row on the `md` step; items line up their labels past the checkbox column. */
const itemClass = (tone: 'default' | 'danger') =>
  cx(
    'flex items-center font-semibold select-none cursor-default',
    ladder.md,
    tone === 'danger' ? 'text-red' : 'text-fg',
    'data-highlighted:bg-surface-2 data-disabled:opacity-50',
    focusRing,
  );

const shortcutText = (shortcut?: string) =>
  shortcut && <span className="ml-auto pl-xl font-normal text-faint">{shortcut}</span>;

/**
 * One action in a Menu, on the `md` step.
 *
 * @useWhen inside Menu only.
 * @states default, highlighted, disabled
 */
export function MenuItem({
  children,
  icon: Icon,
  tone = 'default',
  shortcut,
  disabled = false,
  keepOpen = false,
  onClick,
}: MenuItemProps) {
  return (
    <BaseMenu.Item disabled={disabled} closeOnClick={!keepOpen} onClick={onClick} className={itemClass(tone)}>
      {Icon && <Icon aria-hidden className={iconSize.md} />}
      {children}
      {shortcutText(shortcut)}
    </BaseMenu.Item>
  );
}

export interface MenuCheckboxItemProps {
  /** The setting, as a noun phrase that reads true when ticked ("Start at login"). */
  children: ReactNode;
  /** Whether it is on: what the system reports, never what was asked for. */
  checked: boolean;
  /** Called with the requested state; the menu stays open so the tick shows the outcome. */
  onCheckedChange: (checked: boolean) => void;
  /** Shows it but refuses activation, only while a sibling action in the same place runs. */
  disabled?: boolean;
}

/**
 * An on/off setting in a Menu, on the `md` step, with a tick at the trailing edge when on (so its label
 * lines up with the plain items above and below). The menu stays open after a
 * press, so the tick can show what the system did with the request.
 *
 * @useWhen inside Menu, for a setting the surface has no room for (the popover's Start at login).
 * @states unchecked, checked, highlighted
 */
export function MenuCheckboxItem({ children, checked, onCheckedChange, disabled = false }: MenuCheckboxItemProps) {
  return (
    <BaseMenu.CheckboxItem
      checked={checked}
      onCheckedChange={(next) => onCheckedChange(next)}
      disabled={disabled}
      className={itemClass('default')}
    >
      {children}
      <BaseMenu.CheckboxItemIndicator className="ml-auto pl-xl text-accent-text">
        <Check aria-hidden className={iconSize.md} />
      </BaseMenu.CheckboxItemIndicator>
    </BaseMenu.CheckboxItem>
  );
}

export interface MenuLabelProps {
  /** A fact about what the menu acts on (the popover's version line): read, never chosen. */
  children: ReactNode;
  /** `error`: why the item above it could not act, in red and announced (`role="status"`); it clears on the next interaction. */
  tone?: 'quiet' | 'error';
}

/**
 * A quiet line of text at the head of a Menu that is not an action: the keyboard skips it, and
 * it is never drawn as a disabled item, because nothing about it is unavailable.
 *
 * @useWhen the first line of a Menu, naming what it acts on (`faber v0.3.13`), or, with `tone`
 * `error`, the note under an item that could not act. Not for a heading between groups: separate
 * those with MenuSeparator.
 * @states default, error
 */
export function MenuLabel({ children, tone = 'quiet' }: MenuLabelProps) {
  return (
    <BaseMenu.Group>
      <BaseMenu.GroupLabel
        role={tone === 'error' ? 'status' : undefined}
        className={cx('max-w-full px-md py-2xs text-caption select-text', tone === 'error' ? 'text-red' : 'text-faint')}
      >
        {children}
      </BaseMenu.GroupLabel>
    </BaseMenu.Group>
  );
}

/**
 * A rule between groups of MenuItems.
 *
 * @useWhen inside Menu, between groups of related actions. Never first or last.
 * @states default
 */
export function MenuSeparator() {
  return <BaseMenu.Separator className="my-2xs h-px bg-border-soft" />;
}
