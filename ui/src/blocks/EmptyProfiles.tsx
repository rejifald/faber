import { Button } from '../kit';

export interface EmptyProfilesProps {
  /** Adds a profile and opens it. */
  onAdd: () => void;
  /** The draft has unsaved changes, so the save bar's Save is the one primary and Add profile yields to the outlined secondary. */
  dirty?: boolean;
}

/**
 * The Profiles section with no profile: a ghost row, what a profile is, and Add profile.
 *
 * @useWhen the config holds no `[[profile]]`, so faber cannot take jobs.
 * @states default, dirty
 */
export function EmptyProfiles({ onAdd, dirty = false }: EmptyProfilesProps) {
  return (
    <div className="flex flex-col gap-sm px-lg py-md">
      <div aria-hidden className="flex items-center gap-lg border-b border-dashed border-border-soft pb-md text-faint">
        <span className="font-mono text-caption">1</span>
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="text-body font-semibold">linux</span>
          <span className="font-mono text-caption">faber, linux</span>
        </span>
        <span className="text-small">docker</span>
      </div>
      <h3 className="text-body font-semibold text-fg-strong">A profile is what a job runs in</h3>
      <p className="text-small text-muted">faber can’t take jobs until it has one. Add one, then restart.</p>
      <div className="flex">
        <Button variant={dirty ? 'secondary' : 'primary'} onClick={onAdd}>
          Add profile
        </Button>
      </div>
    </div>
  );
}
