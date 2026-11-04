import { ArrowUpRight, ChevronRight } from 'lucide-react';
import type { ReactNode } from 'react';
import { Icon, RowButton } from '../kit';
import { cx } from '../kit/ladder';

/** One kind of thing folded away: its mark, how many, and the words for it. */
export interface FoldTally {
  key: string;
  /** The kind's glyph: a still pip, or a severity icon. */
  mark: ReactNode;
  count: number;
  /** The kind in words, for the accessible name: `running`, `warning`. */
  label: string;
}

export interface FoldToggleProps {
  /** How many items are folded away. */
  count: number;
  /** Whether the folded items show. The screen keeps it. */
  open: boolean;
  /** Called with the new state when the line is pressed. */
  onOpenChange: (open: boolean) => void;
  /** What is folded, one entry per kind present, in the order drawn. Collapsed only. */
  tally?: FoldTally[];
  /** Open only: adds "Open panel ↗" at the right, which opens the panel window and does not toggle. */
  onPanel?: () => void;
  /** The id of the list this opens, for `aria-controls`. */
  controls?: string;
}

/**
 * The last line of a list the popover folds. Collapsed it reads "Show N more" and tallies what
 * is folded, one still mark and count per kind, so a failure is never only a number; pressed, the
 * list opens in place and the line reads "Show fewer". Open in the jobs column it also offers
 * "Open panel ↗". No fill, radius or accent: a row on the grid of the lines it ends. Never wraps.
 *
 * @useWhen the end of a folded list: after the compact job lines (tally of pips), and after the
 * third attention row, only when two or more are folded (tally of severities).
 * @states collapsed, collapsed with a runner failure folded, collapsed with severities, open, open with panel link, hover, pressed, focus-visible
 */
export function FoldToggle({ count, open, onOpenChange, tally = [], onPanel, controls }: FoldToggleProps) {
  const words = tally.map((t) => `${t.count} ${t.label}`).join(', ');
  const panel = open && onPanel;
  // One tree for both states, so the chevron turns and focus stays on the toggle when it flips.
  return (
    <div className={cx('flex items-center', panel && 'border-t border-border pt-2xs')}>
      <RowButton
        grow
        expanded={open}
        controls={controls}
        label={open ? 'Show fewer' : `Show ${count} more${words ? `: ${words}` : ''}`}
        onClick={() => onOpenChange(!open)}
      >
        <span className="flex min-w-0 flex-1 items-center gap-sm">
          <span
            className={cx('flex text-faint transition-transform group-hover/row:text-fg motion-reduce:transition-none', open && '-rotate-90')}
          >
            <Icon icon={ChevronRight} size="sm" />
          </span>
          {open ? 'Show fewer' : `Show ${count} more`}
        </span>
        {!open && tally.length > 0 && (
          <span aria-hidden className="flex shrink-0 items-center gap-md">
            {tally.map((t) => (
              <span key={t.key} className="flex items-center gap-2xs font-mono text-caption tabular-nums text-muted">
                {t.mark}
                {t.count}
              </span>
            ))}
          </span>
        )}
        {!open && <span aria-hidden className="w-age shrink-0" />}
      </RowButton>
      {panel && (
        <RowButton onClick={panel}>
          Open panel
          <span className="flex text-faint group-hover/row:text-fg">
            <Icon icon={ArrowUpRight} size="sm" />
          </span>
        </RowButton>
      )}
    </div>
  );
}
