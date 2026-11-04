import { bytes, plural } from '../popover/format';
import type { MirrorEcosystemView, MirrorView } from '../popover/types';

/** At or above this share of the cap, the section says eviction is making room. */
const NEAR_CAP = 0.9;

/** npm's counters for requests that came on an address no sync had named yet. */
const UNATTRIBUTED = '(unattributed)';

/** How each ecosystem is worded: what the store keeps of it, and where a miss is fetched from. */
const ECOSYSTEMS = [
  {
    key: 'lfs',
    title: 'Git LFS',
    unit: 'object',
    upstream: 'From GitHub',
    empty: 'No job has fetched an LFS object through the mirror yet.',
  },
  {
    key: 'npm',
    title: 'npm',
    unit: 'tarball',
    upstream: 'From the registry',
    empty: 'No job has installed an npm package through the mirror yet.',
  },
] as const;

export interface MirrorSectionProps {
  /** `/api/state` → `mirror`. The section draws nothing without its `lfs` or `npm` figures. */
  mirror: MirrorView;
}

/**
 * The pull-through mirror's figures since its service started: what the store holds against its
 * one cap, and for each ecosystem (Git LFS, npm) per repo what it served from its cache and what it
 * fetched upstream (ADR-0047 § 10, since CI logs carry no byte counts). A quiet section: the
 * mirror's faults are attention rows, never here.
 *
 * @useWhen the dashboard's Now group, beside the git mirrors (ADR-0041), with `[mirror.lfs]` or
 * `[mirror.npm]` on. Not for the mirror being down or passing through: those are notices in Needs
 * attention.
 * @states empty, populated, cap nearly full, fill off, npm empty, npm only, LFS and npm
 */
export function MirrorSection({ mirror }: MirrorSectionProps) {
  const shown = ECOSYSTEMS.flatMap((e) => {
    const view = mirror[e.key];
    return view ? [{ ...e, view }] : [];
  });
  if (shown.length === 0) return null;
  const cap = shown[0].view.max_bytes;
  const stored = shown.reduce((n, e) => n + e.view.bytes, 0);
  const near = cap > 0 && stored >= cap * NEAR_CAP;
  return (
    <section aria-labelledby="mirror-heading" className="flex flex-col gap-md text-body text-fg">
      <h2 id="mirror-heading" className="text-title font-semibold text-fg-strong">
        Mirror
      </h2>
      <p className="text-small text-muted">
        {bytes(stored)} of {bytes(cap)} stored.
        {near && ' Near the cap: what was used least recently is evicted to make room.'}
      </p>
      {shown.map((e) => (
        <Ecosystem
          key={e.key}
          id={`mirror-${e.key}-heading`}
          title={e.title}
          unit={e.unit}
          upstream={e.upstream}
          empty={e.empty}
          view={e.view}
        />
      ))}
    </section>
  );
}

interface EcosystemProps {
  /** The heading's id, which names the ecosystem's table for assistive tech. */
  id: string;
  title: string;
  unit: string;
  upstream: string;
  empty: string;
  view: MirrorEcosystemView;
}

/** One ecosystem: its share of the store and its per-repo table. */
function Ecosystem({ id, title, unit, upstream, empty, view }: EcosystemProps) {
  return (
    <div className="flex flex-col gap-sm">
      <h3 id={id} className="text-body font-semibold text-fg-strong">
        {title}
      </h3>
      <p className="text-small text-muted">
        {bytes(view.bytes)} in {plural(view.objects, unit)}.
        {view.fill === 'off' && ` Not storing new ${unit}s: fill is off, so a miss is passed through.`}
      </p>
      {view.repos.length === 0 ? (
        <p className="text-small text-muted">{empty}</p>
      ) : (
        <table aria-labelledby={id} className="w-full border-collapse text-small">
          <thead>
            <tr className="border-b border-border-soft text-left text-muted">
              <th scope="col" className="py-xs pr-lg font-medium">
                Repo
              </th>
              <th scope="col" className="py-xs pr-lg text-right font-medium">
                Hits
              </th>
              <th scope="col" className="py-xs pr-lg text-right font-medium">
                Misses
              </th>
              <th scope="col" className="py-xs pr-lg text-right font-medium">
                From cache
              </th>
              <th scope="col" className="py-xs pr-lg text-right font-medium">
                {upstream}
              </th>
              <th scope="col" className="py-xs text-right font-medium">
                Passed through
              </th>
            </tr>
          </thead>
          <tbody>
            {view.repos.map((r) => (
              <tr key={r.repo} className="border-b border-border-soft">
                <th
                  scope="row"
                  className={`py-xs pr-lg text-left font-normal${r.repo === UNATTRIBUTED ? ' text-muted' : ''}`}
                >
                  {r.repo}
                </th>
                <td className="py-xs pr-lg text-right font-mono">{r.hits}</td>
                <td className="py-xs pr-lg text-right font-mono">{r.misses}</td>
                <td className="py-xs pr-lg text-right font-mono">{bytes(r.bytes_from_cache)}</td>
                <td className="py-xs pr-lg text-right font-mono">{bytes(r.bytes_from_upstream)}</td>
                <td className="py-xs text-right font-mono">{r.passed_through}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
