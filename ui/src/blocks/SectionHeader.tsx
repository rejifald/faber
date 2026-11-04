import type { ReactNode } from 'react';

export interface SectionHeaderProps {
  /** The section's title (`SectionSpec.title`). */
  title: string;
  /** What the section is for and when its changes apply (`SectionSpec.hint`). */
  hint?: string;
  /** A control at the right of the title, such as Add profile. */
  action?: ReactNode;
}

/**
 * The head of a Settings section: its title and the hint under it.
 *
 * @useWhen once at the top of each section of the Settings view, from its SectionSpec. Not for a
 * group of fields inside a section; those use a plain caption.
 * @states default, with action
 */
export function SectionHeader({ title, hint, action }: SectionHeaderProps) {
  return (
    <header className="flex items-start gap-lg">
      <div className="flex min-w-0 flex-1 flex-col gap-2xs">
        <h2 className="text-body font-semibold text-fg-strong">{title}</h2>
        {hint && <p className="text-caption text-muted">{hint}</p>}
      </div>
      {action}
    </header>
  );
}
