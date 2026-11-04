import { worstPip, type Group } from '../popover/groups';
import { Pipe } from './Pipe';
import { PipStrip } from './PipStrip';

export interface CompactGroupProps {
  /** The folded group: its name, pips (only the worst is drawn), age, label and title. */
  group: Group;
}

/**
 * A job group folded past the gauge column's height, on one line: its name, one pip for its
 * worst state (runner failed, frozen, waiting, running, done, in that order) and
 * its age. The pip is the full row's glyph, so a folded group reads like a full one. Not
 * pressable: the "Show N more" line after the last one opens the list in place.
 *
 * @useWhen the popover's two-column jobs column, for each group past those that fit in full (popover/anatomy.md § 6).
 * @states running, waiting, fault, frozen, done
 */
export function CompactGroup({ group }: CompactGroupProps) {
  const pip = worstPip(group);
  return (
    <div role="group" aria-label={group.label} title={group.title} className="flex items-center gap-sm px-xl py-3xs text-small">
      <span className="min-w-0 flex-1 truncate text-muted">
        {group.name}
        {group.workflow && (
          <>
            <Pipe />
            {group.workflow}
          </>
        )}
      </span>
      {pip && <PipStrip pips={[pip]} />}
      <span className="w-age shrink-0 text-right font-mono text-caption whitespace-nowrap text-faint">{group.age}</span>
    </div>
  );
}
