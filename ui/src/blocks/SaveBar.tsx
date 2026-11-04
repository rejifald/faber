import { Button } from '../kit';

export interface SaveBarChange {
  key: string;
  /** The schema's label for the key. */
  label: string;
  /** Applies at once; otherwise on restart. */
  live: boolean;
}

export interface SaveBarProps {
  /** The unsaved changes; none hides the bar. */
  changes: SaveBarChange[];
  /** The save is in flight: Save loads, Discard waits. */
  saving?: boolean;
  /** Replaces the built copy, for a screen that composes its own (the raw editor's "2 lines changed. ..."). */
  summary?: string;
  /** Drops every unsaved change. */
  onDiscard: () => void;
  /** Saves the draft. */
  onSave: () => void;
}

const MAX_NAMED = 2;

const join = (labels: string[]) => labels.join(', ');

/** "1 unsaved change. Applies now." and its longer forms (anatomy.md § 5, SaveBar). */
export function saveBarText(changes: SaveBarChange[]): string {
  const n = changes.length;
  const count = `${n} unsaved ${n === 1 ? 'change' : 'changes'}.`;
  const live = changes.filter((c) => c.live);
  const restart = changes.filter((c) => !c.live);
  if (restart.length === 0) return `${count} Applies now.`;
  if (live.length === 0) return `${count} Applies on restart.`;
  const named = changes.slice(0, MAX_NAMED);
  const more = n - named.length;
  const parts: string[] = [];
  for (const [effect, group] of [
    ['now', named.filter((c) => c.live)],
    ['on restart', named.filter((c) => !c.live)],
  ] as const) {
    if (group.length) parts.push(`${join(group.map((c) => c.label))} ${group.length === 1 ? 'applies' : 'apply'} ${effect}`);
  }
  return `${count} ${parts.join(', ')}${more > 0 ? ` and ${more} more` : ''}.`;
}

/**
 * The raised bar that holds unsaved changes, with Discard and Save.
 *
 * @useWhen the Settings form or the raw editor has a draft. Hidden (render nothing) with no change.
 * @states shown, saving
 */
export function SaveBar({ changes, saving = false, summary, onDiscard, onSave }: SaveBarProps) {
  if (changes.length === 0) return null;
  return (
    <div
      role="region"
      aria-label="Unsaved changes"
      className="flex items-center gap-lg rounded-md border border-border bg-elevate px-lg py-sm shadow-floating"
    >
      <p aria-live="polite" className="min-w-0 flex-1 text-small text-fg">
        {summary ?? saveBarText(changes)}
      </p>
      <Button variant="link" disabled={saving} onClick={onDiscard}>
        Discard
      </Button>
      <Button variant="primary" loading={saving} onClick={onSave}>
        Save
      </Button>
    </div>
  );
}
