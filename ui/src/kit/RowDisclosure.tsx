import { Collapsible } from '@base-ui/react/collapsible';
import { ChevronRight } from 'lucide-react';
import type { ReactNode } from 'react';
import { buttonClass } from './Button';
import { cx, iconSize } from './ladder';

const WASH = { amber: 'bg-amber-soft', red: 'bg-red-soft' };

export interface RowDisclosureProps {
  /** The toggle's accessible name, in words: `Details for acme/mobile build`. */
  label: string;
  /** Whether the detail shows. Set it with `onOpenChange` to control the row; leave it out to let the row keep its own state. */
  open?: boolean;
  /** Whether the detail shows at first, when the row keeps its own state. Default shut. */
  defaultOpen?: boolean;
  /** Called with the new state when the toggle is pressed. */
  onOpenChange?: (open: boolean) => void;
  /** Draws the summary `<tr>`; put the `toggle` it receives in the row's lead cell. */
  row: (toggle: ReactNode) => ReactNode;
  /** What opens under the row: the content of one cell that spans the table. */
  detail: ReactNode;
  /** How many columns the table has, including the lead cell, so the detail spans them all. */
  colSpan: number;
  /** The soft wash of the detail row, to match a toned summary row. */
  tone?: 'amber' | 'red';
}

/**
 * A table row that opens a detail row under it, wrapping Base UI's Collapsible. Root is a
 * `<tbody>` (one row group per row), the toggle is a square ghost button with a chevron that
 * turns down while open, and the panel is a `<tr>` with one cell that spans the table. The kit's
 * Disclosure is a div row and cannot sit in a table.
 *
 * @useWhen a table row whose detail is a few lines more of the same thing, such as a run's
 * reason. For a list row outside a table use Disclosure.
 * @states closed, hover, focus-visible, open
 */
export function RowDisclosure({
  label,
  open,
  defaultOpen,
  onOpenChange,
  row,
  detail,
  colSpan,
  tone,
}: RowDisclosureProps) {
  const toggle = (
    <Collapsible.Trigger
      aria-label={label}
      className={cx('group/toggle', buttonClass('md', 'ghost', true))}
    >
      <ChevronRight aria-hidden className={cx(iconSize.md, 'shrink-0 group-data-panel-open/toggle:rotate-90')} />
    </Collapsible.Trigger>
  );
  return (
    <Collapsible.Root render={<tbody />} open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange}>
      {row(toggle)}
      <Collapsible.Panel render={<tr className={tone ? WASH[tone] : undefined} />}>
        <td colSpan={colSpan} className="px-lg pb-md text-small text-muted wrap-anywhere">
          {detail}
        </td>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
}
