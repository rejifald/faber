import type { HTMLAttributes, Ref } from 'react';

// The control ladder (ADR-0039 § 1, ADR-0042 § 4) as class lists: one step sets
// a control's height, side padding, label size, icon size, gap and radius
// together. Every sized kit control reads these; a screen picks `size` and
// never sets a dimension. A missing size is a change to ADR-0039's table.
// Heights are minimums and icons scale with the label (ADR-0039 § 1, amended
// 2026-10-18), so enlarged text grows a control instead of crowding it.

/** A step of the control ladder. */
export type Size = 'sm' | 'md' | 'lg';

/** Minimum height, side padding, label, gap and radius for a control with a label. */
export const ladder: Record<Size, string> = {
  sm: 'min-h-control-sm px-sm gap-2xs text-caption rounded-sm',
  md: 'min-h-control-md px-md gap-xs text-small rounded-sm',
  lg: 'min-h-control-lg px-lg gap-xs text-body rounded-md',
};

/** The square variant: at least as wide as it is tall, with no side padding. */
export const square: Record<Size, string> = {
  sm: 'min-h-control-sm min-w-control-sm justify-center text-caption rounded-sm',
  md: 'min-h-control-md min-w-control-md justify-center text-small rounded-sm',
  lg: 'min-h-control-lg min-w-control-lg justify-center text-body rounded-md',
};

/** The icon size of each step, for a lucide icon inside the control; it scales with the label. */
export const iconSize: Record<Size, string> = {
  sm: 'size-icon-sm',
  md: 'size-icon-md',
  lg: 'size-icon-lg',
};

/**
 * A control drawn as a box beside its label (Checkbox): the step's label size and icon-to-label
 * gap, with the box the size of the step's icon. No height: the row is as tall as its label.
 */
export const boxed: Record<Size, string> = {
  sm: 'gap-2xs text-caption',
  md: 'gap-xs text-small',
  lg: 'gap-xs text-body',
};

/**
 * The keyboard focus ring every interactive kit part shares. Its base is `outline-hidden`, not
 * Tailwind's none variant, which sets the outline style to none for every state, focus included.
 */
export const focusRing =
  'outline-hidden focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-text';

/** The same ring drawn 2 px inside the element, for a full-bleed row that has no room outside it. */
export const focusRingInset =
  'outline-hidden focus-visible:outline-solid focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent-text';

/**
 * What a kit control passes through to its element besides its own props: the ref and the
 * attributes and handlers a wrapper such as Tooltip or a Menu trigger adds. Never `className` or
 * `style`, so a screen cannot size or colour a control.
 */
export type PassThrough<E extends HTMLElement> = Omit<
  HTMLAttributes<E>,
  'className' | 'style' | 'children' | 'color' | 'onClick'
> & { ref?: Ref<E> };

/** Joins class lists, skipping the falsy ones. */
export function cx(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ');
}
