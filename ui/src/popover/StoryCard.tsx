import type { ReactNode } from 'react';

/** A block's stories render on the popover card's surface and width, as the screen places them. */
export function StoryCard({ children, inset = true }: { children: ReactNode; inset?: boolean }) {
  return (
    <div className={`flex w-popover flex-col gap-px rounded-lg border border-border bg-bg font-sans text-body text-fg ${inset ? 'p-xs' : ''}`}>
      {children}
    </div>
  );
}
