import type { ReactNode } from 'react';
import { StatusDot } from '../kit';
import { Pipe } from './Pipe';

export interface PageHeaderProps {
  /** The view's name, the page's `h1`. */
  title: string;
  /** A dot and a word beside the title: `true` is a green "Live", `'stale'` an amber "Stale". Omit for no indicator. */
  live?: boolean | 'stale';
  /** A short count or fact after the live indicator, such as `17 events`, set in quiet caption ink and divided from the indicator by a pipe. */
  meta?: ReactNode;
  /** What sits at the right of the row, such as a Button and a SplitButton. */
  actions?: ReactNode;
}

/**
 * A dashboard view's title row: the title, a live indicator, a meta line and the view's actions.
 *
 * @useWhen once at the top of each dashboard view. Not for a view pushed in place of another; use ViewHeader.
 * @states default, live, stale, with meta, with actions
 */
export function PageHeader({ title, live, meta, actions }: PageHeaderProps) {
  return (
    <header className="flex w-full items-center gap-md">
      <h1 className="min-w-0 text-title font-semibold text-fg-strong">{title}</h1>
      {live !== undefined && live !== false && (
        <span className="flex shrink-0 items-center gap-xs text-caption text-faint">
          <StatusDot tone={live === 'stale' ? 'amber' : 'green'} />
          {live === 'stale' ? 'Stale' : 'Live'}
        </span>
      )}
      {meta !== undefined && meta !== null && meta !== false && (
        <span className="flex min-w-0 items-center text-caption text-faint">
          {live !== undefined && live !== false && <Pipe />}
          {meta}
        </span>
      )}
      {actions && <div className="ml-auto flex shrink-0 items-center gap-sm">{actions}</div>}
    </header>
  );
}
