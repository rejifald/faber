import { Search } from 'lucide-react';
import { focusRing, Input, Select } from '../kit';
import { groupedNav, type NavItem } from '../settings/nav';

/** One search hit, drawn "Section › Field". */
export interface NavResult {
  /** The id of the section the hit is in. */
  id: string;
  section: string;
  field: string;
}

export interface SettingsNavProps {
  /** The sections, in order, each with its group and tag. */
  items: NavItem[];
  /** The id of the selected section. */
  selected: string;
  /** Called with the chosen section id. */
  onSelect: (id: string) => void;
  /** The search text. */
  query: string;
  /** Called as the search text changes. */
  onQuery: (query: string) => void;
  /** The hits for `query`; with a query and none, the nav says nothing matches. */
  results?: NavResult[];
}

const link =
  `flex items-center justify-between gap-sm rounded-sm px-sm py-xs text-small ${focusRing}`;

/**
 * The section list beside the Settings form, with search on top: groups with their sections, a
 * `live` tag where changes apply at once, the profile count on Profiles. A search shows its hits in
 * place of the groups. Below 640 px it is a Select of the sections under the search field.
 *
 * @useWhen the one navigation of the Settings view. Not for peer views of one screen; use Tabs.
 * @states default, item hover, selected, focus-visible, searching with hits, no hits
 */
export function SettingsNav({ items, selected, onSelect, query, onQuery, results = [] }: SettingsNavProps) {
  const searching = query.trim() !== '';
  const go = (id: string) => (e: { preventDefault: () => void }) => {
    e.preventDefault();
    onSelect(id);
  };
  return (
    <nav
      aria-label="Settings sections"
      className="flex flex-col gap-lg border-b border-border bg-surface-2 p-lg narrow:w-nav narrow:shrink-0 narrow:border-r narrow:border-b-0"
    >
      <label className="block">
        <span className="sr-only">Search settings</span>
        <Input type="search" leadingIcon={Search} value={query} onValueChange={onQuery} placeholder="Search settings" />
      </label>
      {searching ? (
        results.length === 0 ? (
          <p role="status" className="px-sm text-small text-muted">
            No setting matches
          </p>
        ) : (
          <ul className="flex flex-col gap-3xs">
            {results.map((r) => (
              <li key={`${r.id}:${r.field}`}>
                <a href={`#${r.id}`} onClick={go(r.id)} className={`${link} text-fg hover:bg-border-soft`}>
                  {`${r.section} › ${r.field}`}
                </a>
              </li>
            ))}
          </ul>
        )
      ) : (
        <>
          <div className="narrow:hidden">
            <Select
              value={selected}
              onValueChange={onSelect}
              options={items.map((i) => ({ value: i.id, label: i.title }))}
            />
          </div>
          <div className="hidden flex-col gap-lg narrow:flex">
            {groupedNav(items).map(({ group, items: members }) => (
              <div key={group} className="flex flex-col gap-3xs">
                <span className="px-sm pb-2xs text-caption font-semibold text-muted">{group}</span>
                <ul className="flex flex-col gap-3xs">
                  {members.map((i) => (
                    <li key={i.id}>
                      <a
                        href={`#${i.id}`}
                        onClick={go(i.id)}
                        aria-current={i.id === selected ? 'page' : undefined}
                        className={`${link} ${i.id === selected ? 'bg-surface font-semibold text-fg-strong' : 'text-fg hover:bg-border-soft'}`}
                      >
                        {i.title}
                        {i.tag && <span className="text-caption font-normal text-muted">{i.tag}</span>}
                      </a>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        </>
      )}
    </nav>
  );
}
