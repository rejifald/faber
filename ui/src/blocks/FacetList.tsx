import { StatusDot, ToggleRow, type StatusTone } from '../kit';

/** One value in a group: its label, how many rows carry it and optionally a state dot. */
export interface FacetItem {
  /** The value, passed to `onChange` and matched against `selected`. */
  value: string;
  /** The value in words. Daemon text is passed as strings and set as text. */
  label: string;
  /** How many rows carry this value. */
  count: number;
  /** Shows a state dot before the label. */
  tone?: StatusTone;
}

/** A named group of values. */
export interface FacetGroup {
  /** Stable id, the key into `selected`. */
  id: string;
  /** The group's name, in sentence case; it names the group for assistive tech. */
  label: string;
  /** The values, in order. A group with none is not drawn. */
  items: FacetItem[];
}

export interface FacetListProps {
  /** The groups, in order. */
  groups: FacetGroup[];
  /** The chosen values of each group by group id; any number per group, none for a missing id. */
  selected: Record<string, string[]>;
  /** Called with the group id and its new list of chosen values when an item is pressed. */
  onChange: (groupId: string, values: string[]) => void;
}

/**
 * The filter column of a list: groups of values, each with a count, any number of which can be
 * chosen. Each item is a toggle button; a group is named by its label.
 *
 * @useWhen narrowing a long list by several facets at once, such as Activity by kind, result,
 * repo and actor. Not for one choice among a few views (SegmentedControl or Tabs).
 * @states default, item hover, item selected, focus-visible, with dots, empty group hidden
 */
export function FacetList({ groups, selected, onChange }: FacetListProps) {
  return (
    <div className="flex flex-col gap-xl rounded-lg border border-border bg-surface p-lg">
      {groups
        .filter((g) => g.items.length > 0)
        .map((g) => {
          const chosen = selected[g.id] ?? [];
          return (
            <div key={g.id} role="group" aria-label={g.label} className="flex flex-col gap-3xs">
              <div className="px-sm pb-2xs text-caption font-semibold text-muted">{g.label}</div>
              {g.items.map((i) => {
                const on = chosen.includes(i.value);
                return (
                  <ToggleRow
                    key={i.value}
                    pressed={on}
                    weight="regular"
                    onPressedChange={(next) => onChange(g.id, next ? [...chosen, i.value] : chosen.filter((v) => v !== i.value))}
                  >
                    {i.tone && <StatusDot tone={i.tone} />}
                    <span className="min-w-0 flex-1 truncate text-left">{i.label}</span>
                    {/* Faint fails contrast on the accent wash, so an on row's count takes the accent text. */}
                    <span className="font-mono text-caption font-normal text-faint group-data-pressed/toggle:text-accent-text">{i.count}</span>
                  </ToggleRow>
                );
              })}
            </div>
          );
        })}
    </div>
  );
}
