import { useId, type ReactNode } from 'react';

export interface PanelProps {
  /** The section's heading, an `h2`. */
  title: string;
  /** The quieter line after the title, such as "3 admitted, 1 parked". */
  meta?: ReactNode;
  /** What sits at the right of the heading line: a SegmentedControl or a count. */
  action?: ReactNode;
  /** The section's content. When it is empty, it says why; the panel never invents copy. */
  children: ReactNode;
  /** Heading only: the children are not carded, for a list of cards. */
  bare?: boolean;
}

/**
 * A dashboard section: its heading line and the card that holds its content.
 *
 * @useWhen grouping one dashboard section under a heading, such as In flight or Activity. Not for
 * a Settings section; use SectionHeader.
 * @states default, with meta, with action, bare, empty
 */
export function Panel({ title, meta, action, children, bare = false }: PanelProps) {
  const id = useId();
  return (
    <section aria-labelledby={id} className="flex w-full flex-col gap-sm">
      <div className="flex items-center gap-md">
        <h2 id={id} className="text-body font-semibold text-fg-strong">
          {title}
        </h2>
        {meta && <div className="min-w-0 text-small text-muted">{meta}</div>}
        {action && <div className="ml-auto flex shrink-0 items-center gap-sm">{action}</div>}
      </div>
      {bare ? children : <div className="rounded-lg border border-border bg-surface">{children}</div>}
    </section>
  );
}
