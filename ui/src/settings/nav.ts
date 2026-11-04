import type { FormSchema, SectionSpec } from './types';

// The Settings sidebar's sections and groups (anatomy.md § 5, SettingsNav). The daemon's section
// titles are mapped to a group here; a title the map does not know goes last in `faber`, so a new
// daemon section still shows. Profiles is not a schema section: the screen adds it.

export const GROUPS = ['Resources', 'Jobs', 'faber'] as const;
export type NavGroup = (typeof GROUPS)[number];

/** The id of the Profiles section, which the schema does not list. */
export const PROFILES_ID = 'profiles';
/** The id of the raw `config.toml` editor, the last item of the nav. */
export const RAW_ID = 'config-toml';

/** The known sections in the order they show, each with its group. */
const KNOWN: { title: string; group: NavGroup }[] = [
  { title: 'Limits', group: 'Resources' },
  { title: 'Admission tuning', group: 'Resources' },
  { title: 'Docker host', group: 'Resources' },
  { title: 'Linux VM', group: 'Resources' },
  { title: 'Trust', group: 'Jobs' },
  { title: 'Profiles', group: 'Jobs' },
  { title: 'Runner images', group: 'Jobs' },
  { title: 'Runner-image drift', group: 'Jobs' },
  { title: 'Auto-retry', group: 'Jobs' },
  { title: 'Registry auth', group: 'Jobs' },
  { title: 'Daemon', group: 'faber' },
  { title: 'Updates', group: 'faber' },
];

export interface NavItem {
  id: string;
  title: string;
  group: NavGroup;
  /** `live` where a section's settings apply at once, or the profile count on Profiles. */
  tag?: string;
}

/** A section's id: its title lower-cased, runs of other characters made one hyphen. */
export function sectionId(title: string): string {
  return title.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
}

/** The group a section title belongs to; an unknown title is in `faber`. */
export function groupOf(title: string): NavGroup {
  return KNOWN.find((k) => k.title === title)?.group ?? 'faber';
}

/** What the page shows for the daemon's Autolimiter section's untimed half. */
const TUNING_HINT = 'Learned sizes and CPU admission.';

/** A section as the Settings view shows it: its nav id and group with the spec's title, hint and fields. */
export interface ViewSection extends SectionSpec {
  id: string;
  group: NavGroup;
}

/**
 * The sections the view shows. The daemon's Autolimiter is two: Limits (its `live` fields) and
 * Admission tuning (the rest); either is left out when it has no field. Every other section is as
 * the daemon sent it. Ordered by group and the known order, unknown sections last in `faber`.
 */
export function viewSections(schema: FormSchema): ViewSection[] {
  const out: SectionSpec[] = [];
  for (const s of schema.sections) {
    if (s.title !== 'Autolimiter') {
      out.push(s);
      continue;
    }
    const live = s.fields.filter((f) => f.live);
    const rest = s.fields.filter((f) => !f.live);
    if (live.length) out.push({ title: 'Limits', hint: s.hint, fields: live });
    if (rest.length) out.push({ title: 'Admission tuning', hint: TUNING_HINT, fields: rest });
  }
  const rank = (t: string) => {
    const at = KNOWN.findIndex((k) => k.title === t);
    return at < 0 ? KNOWN.length : at;
  };
  return out
    .map((s, at) => ({ s, at }))
    .sort((a, b) => rank(a.s.title) - rank(b.s.title) || a.at - b.at)
    .map(({ s }) => ({ ...s, id: sectionId(s.title), group: groupOf(s.title) }));
}

/**
 * The nav items for a schema: its view sections plus Profiles (tagged with `profileCount`), ordered
 * by group and the known order, unknown sections last in `faber`. `raw` adds `config.toml` last.
 */
export function navItems(schema: FormSchema, profileCount: number, raw = false): NavItem[] {
  const items: NavItem[] = viewSections(schema).map((s) => ({
    id: s.id,
    title: s.title,
    group: s.group,
    ...(s.fields.some((f) => f.live) ? { tag: 'live' } : {}),
  }));
  items.push({ id: PROFILES_ID, title: 'Profiles', group: 'Jobs', tag: String(profileCount) });
  const rank = (i: NavItem) => {
    const at = KNOWN.findIndex((k) => k.title === i.title);
    return at < 0 ? KNOWN.length : at;
  };
  const known = items.map((item, at) => ({ item, at })).sort((a, b) => rank(a.item) - rank(b.item) || a.at - b.at).map((x) => x.item);
  if (raw) known.push({ id: RAW_ID, title: 'config.toml', group: 'faber' });
  return known;
}

/** The items under their group labels, in group order, empty groups left out. */
export function groupedNav(items: NavItem[]): { group: NavGroup; items: NavItem[] }[] {
  return GROUPS.map((group) => ({ group, items: items.filter((i) => i.group === group) })).filter((g) => g.items.length > 0);
}
