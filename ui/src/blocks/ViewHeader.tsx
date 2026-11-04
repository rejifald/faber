import { ChevronLeft, ChevronRight } from 'lucide-react';
import { useEffect, useRef, type ReactNode } from 'react';
import { Button, Icon, IconButton } from '../kit';

export interface ViewHeaderProps {
  /** The view's name, a heading. */
  title: string;
  /** Goes back to the view this one replaced; shows the Back button, and focus lands on it on mount. */
  onBack?: () => void;
  /** What sits at the right: the strip passes "Applies now" with a green dot. */
  status?: ReactNode;
  /** The view this one sits under, drawn as a link before the title: "Profiles ›". */
  crumb?: { label: string; onClick: () => void };
  /** What sits at the right of the title, such as Remove. */
  action?: ReactNode;
}

/**
 * The header of a view pushed in place of another: back, title, an optional status at the right.
 *
 * @useWhen a view replaces the popover body and the reader must be able to return, as the Limits strip does.
 * @states default, crumb, back hover, back focus-visible
 */
export function ViewHeader({ title, onBack, status, crumb, action }: ViewHeaderProps) {
  const back = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (onBack) back.current?.focus();
    // Only on mount: the keyboard lands in the new view once.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return (
    <header className="flex items-center gap-sm border-b border-border-soft px-lg py-md">
      {onBack && <IconButton ref={back} icon={ChevronLeft} label="Back" onClick={() => onBack()} />}
      {crumb && (
        <>
          <Button variant="link" size="sm" onClick={crumb.onClick}>
            {crumb.label}
          </Button>
          <span className="text-muted">
            <Icon icon={ChevronRight} />
          </span>
        </>
      )}
      <h2 className="min-w-0 flex-1 truncate text-body font-semibold text-fg-strong">{title}</h2>
      {status && <div className="flex shrink-0 items-center gap-xs text-caption text-green">{status}</div>}
      {action && <div className="flex shrink-0 items-center gap-sm">{action}</div>}
    </header>
  );
}
