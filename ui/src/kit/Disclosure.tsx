import { Collapsible } from '@base-ui/react/collapsible';
import type { ReactNode } from 'react';
import { cx, focusRingInset } from './ladder';

export interface DisclosureProps {
  /** The row itself: what shows while it is shut. Its parts keep their own colours on hover: the tint is neutral. */
  summary: ReactNode;
  /** The row's accessible name, in words: what the summary's marks say (`demo/PR-2741, 3 running, 6 minutes`). */
  label: string;
  /** Free text shown on hover: identity strings too long for the row. */
  title?: string;
  /** Whether the content shows. Controlled: the screen keeps one open at a time. */
  open: boolean;
  /** Called with the new state when the row is pressed. */
  onOpenChange: (open: boolean) => void;
  /** What opens under the row, on the same fill. */
  children: ReactNode;
}

/**
 * A row that opens in place to show what is under it, wrapping Base UI's Collapsible. Drawn as a
 * Mail-style list row (#369): full bleed, a neutral lift on hover (`row-hover`), a stronger one while
 * pressed (`row-press`), and the open row and its content share the hover tint as one block. Its
 * parent sets no side padding; the row's own 16 px lines its text up with the card's. The focus
 * ring is drawn inside the row, since a full-bleed row has no room outside it.
 *
 * @useWhen a list row whose detail is a few lines more of the same thing: a job group and its
 * jobs. For actions use Menu; for a page section use the surface's own heading.
 * @states closed, hover, pressed, open, focus-visible
 */
export function Disclosure({ summary, label, title, open, onOpenChange, children }: DisclosureProps) {
  return (
    <Collapsible.Root open={open} onOpenChange={onOpenChange}>
      <Collapsible.Trigger
        aria-label={label}
        title={title}
        className={cx(
          'group/disclosure flex w-full flex-col items-stretch gap-3xs px-xl py-xs text-left text-body',
          'cursor-default select-none text-fg',
          'hover:bg-row-hover data-panel-open:bg-row-hover active:bg-row-press',
          focusRingInset,
        )}
      >
        {summary}
      </Collapsible.Trigger>
      <Collapsible.Panel className="flex flex-col bg-row-hover pb-2xs">{children}</Collapsible.Panel>
    </Collapsible.Root>
  );
}
