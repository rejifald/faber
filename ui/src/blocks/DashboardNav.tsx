import type { LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { focusRing, Icon, Mark, Select } from '../kit';

/** One destination in the dashboard's sidebar. */
export interface DashboardNavItem {
  /** Stable id, passed to `onSelect` and matched against `selected`. */
  id: string;
  /** The row's label, in sentence case. */
  label: string;
  /** A lucide icon before the label. */
  icon: LucideIcon;
  /** Where the row goes: a path in `page` mode, a `#fragment` in `section` mode. */
  href: string;
  /** A count or short figure at the trailing edge; omit for none. */
  count?: ReactNode;
  /** What the count is: a `number` (mono, the default) or a `word` such as "Paused" (the sans face). Stated, never guessed from the text. */
  countKind?: 'number' | 'word';
  /** `amber` or `red` turns the count loud; omit for a quiet count. On the selected row every
   * count takes the row's accent text: faint and red fail contrast on the accent wash. */
  countTone?: 'amber' | 'red';
  /** The count in words ("7 faults"), added to the link's accessible name. */
  countLabel?: string;
}

export interface DashboardNavProps {
  /** The destinations, in order. The IA is the screen's; nothing here names a view. */
  items: DashboardNavItem[];
  /** The id of the selected item. */
  selected: string;
  /** `page`: items are separate views (`aria-current="page"`). `section`: items jump within one page (`aria-current="location"`). */
  mode: 'page' | 'section';
  /** Called with the chosen id, as a row is pressed or the narrow Select changes. With it, a plain click routes in place (the link's own navigation is cancelled; a modified click still opens the `href` in a new tab); without it, rows and the narrow Select navigate to the item's `href`. */
  onSelect?: (id: string) => void;
  /** The faber version, drawn beside the name. */
  version: string;
  /** The host card, under the brand (a HostCard). */
  host: ReactNode;
  /** The foot of the sidebar (an UpdateCard); omit for none. */
  footer?: ReactNode;
}

const link =
  `flex items-center gap-sm rounded-sm px-sm py-xs text-body ${focusRing}`;

const countTone = { amber: 'font-semibold text-amber', red: 'font-semibold text-red' };

/**
 * The dashboard shell's sidebar: the faber mark with its version, the host card, the destinations
 * as icon, label and trailing count, and an update card at the foot. It takes its items as data and
 * names no view, so the screen decides the IA. Below 640 px the list becomes a Select of the items.
 *
 * @useWhen the one navigation of the dashboard shell. Not for peer views of one screen (use Tabs)
 * and not for Settings sections (use SettingsNav).
 * @states default, item hover, selected, focus-visible, with counts, with a word count, with footer, no footer
 */
export function DashboardNav({ items, selected, mode, onSelect, version, host, footer }: DashboardNavProps) {
  const current = mode === 'page' ? 'page' : 'location';
  return (
    <div className="flex flex-col gap-xl border-b border-border bg-surface-2 p-lg narrow:w-sidebar narrow:shrink-0 narrow:border-r narrow:border-b-0">
      <div className="flex items-center gap-sm px-sm">
        <Mark />
        <span className="text-title font-semibold text-fg-strong">faber</span>
        <span className="font-mono text-caption text-faint">{version}</span>
      </div>
      {host}
      <nav aria-label="Dashboard">
        <div className="narrow:hidden">
          <Select value={selected} onValueChange={(id) => {
              if (onSelect) onSelect(id);
              else {
                const it = items.find((i) => i.id === id);
                if (it) window.location.assign(it.href);
              }
            }} options={items.map((i) => ({ value: i.id, label: i.label }))} />
        </div>
        <ul className="hidden flex-col gap-3xs narrow:flex">
          {items.map((i) => {
            const isSelected = i.id === selected;
            return (
              <li key={i.id}>
                <a
                  href={i.href}
                  onClick={
                    onSelect
                      ? (e) => {
                          // A plain click routes in place; a modified one (new tab, new window) is the browser's.
                          if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
                          e.preventDefault();
                          onSelect(i.id);
                        }
                      : undefined
                  }
                  aria-current={isSelected ? current : undefined}
                  aria-label={i.countLabel ? `${i.label}, ${i.countLabel}` : undefined}
                  className={`${link} ${isSelected ? 'bg-accent-soft font-semibold text-accent-text' : 'text-fg hover:bg-border-soft'}`}
                >
                  <span className={isSelected ? '' : 'text-muted'}>
                    <Icon icon={i.icon} />
                  </span>
                  {i.label}
                  {i.count !== undefined && (
                    <span
                      className={`ml-auto ${i.countKind === 'word' ? 'font-sans' : 'font-mono'} text-caption ${isSelected ? 'text-accent-text' : i.countTone ? countTone[i.countTone] : 'font-normal text-faint'}`}
                    >
                      {i.count}
                    </span>
                  )}
                </a>
              </li>
            );
          })}
        </ul>
      </nav>
      {footer && <div className="mt-auto">{footer}</div>}
    </div>
  );
}
