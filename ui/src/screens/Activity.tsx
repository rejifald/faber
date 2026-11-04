import { Search } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { CodeBlock } from '../blocks/CodeBlock';
import { DataTable, type DataTableColumn, type DataTableRow } from '../blocks/DataTable';
import { FacetList } from '../blocks/FacetList';
import { KeyValueList } from '../blocks/KeyValueList';
import { RunPhases } from '../blocks/RunPhases';
import { PageHeader } from '../blocks/PageHeader';
import {
  applyFilters,
  buildEvents,
  buildFacets,
  emptyFilters,
  isFiltered,
  KIND_LABEL,
  parseQuery,
  readLocationQuery,
  toQuery,
  whenLabel,
  writeLocationQuery,
  type ActivityEvent,
  type EventTone,
  type FacetId,
  type Filters,
} from '../dashboard/activity';
import { getRun, type RunFootprint } from '../dashboard/jobs';
import type { ViewProps } from '../dashboard/screen';
import { Button, Input, StatusBadge } from '../kit';

export interface ActivityProps extends ViewProps {
  /** The last poll is old or failed: the header says "Stale". */
  stale?: boolean;
  /** The filters as a query string (`?kind=fault`). Given, the view starts from it and leaves the address bar alone; omitted, it reads and writes `location.search` (ADR-0054 § 5). */
  initialQuery?: string;
  /** Opens the first row that carries a message at first, for a story. */
  openFirstMessage?: boolean;
  /** Opens the first finished run at first, for a story. */
  openFirstRun?: boolean;
  /** Reads a finished run's phases when its row opens. The daemon's `/api/runs/:runner` when omitted; stories pass fixtures. */
  loadRun?: (runner: string) => Promise<RunFootprint | null>;
}

// Only faults and drift tint their Detail: a refusal or a bypass is read from its badge, so its text stays muted.
const TINTED: ReadonlySet<ActivityEvent['kind']> = new Set(['outcome', 'fault', 'drift']);
const DETAIL_TONE: Partial<Record<EventTone, string>> = { amber: 'text-amber', red: 'text-red' };
const detailTone = (e: ActivityEvent) => (TINTED.has(e.kind) && DETAIL_TONE[e.tone]) || 'text-muted';

const COLUMNS: DataTableColumn[] = [
  { key: 'when', header: 'When', width: 'tight' },
  { key: 'kind', header: 'Kind', width: 'tight' },
  { key: 'status', header: 'Status', width: 'tight' },
  { key: 'repo', header: 'Repo' },
  { key: 'job', header: 'Job', mono: true },
  { key: 'detail', header: 'Detail', width: 'fill' },
  { key: 'took', header: 'Took', width: 'tight', align: 'end', mono: true },
];

function toRow(e: ActivityEvent, now: number, open: boolean, loadRun: ActivityProps['loadRun']): DataTableRow {
  const expandable = e.message !== undefined || e.fields.length > 0;
  const where = [e.repo, e.job].filter(Boolean).join(' ');
  return {
    key: e.key,
    // A retry, or a run that took a second attempt, carries an amber bar at its lead edge.
    ...(e.kind === 'retry' || (e.attempt ?? 1) > 1 ? { bar: 'amber' as const } : {}),
    cells: {
      when: <span className="text-muted">{whenLabel(e, now)}</span>,
      kind: <span className="text-caption text-faint">{KIND_LABEL[e.kind]}</span>,
      status: <StatusBadge tone={e.tone}>{e.result}</StatusBadge>,
      repo: e.repo ? (
        <>
          {e.repo}
          {e.pr !== undefined && <span className="text-faint"> #{e.pr}</span>}
        </>
      ) : (
        <span className="text-faint">—</span>
      ),
      job: e.job || <span className="text-faint">—</span>,
      detail: <span className={detailTone(e)}>{e.detail}</span>,
      took: e.took ?? <span className="text-faint">—</span>,
    },
    detail: expandable ? (
      <div className="flex flex-col gap-md pt-xs">
        {e.kind === 'outcome' && e.runner && loadRun && <RunPhases runner={e.runner} load={loadRun} />}
        {e.message !== undefined && <CodeBlock>{e.message}</CodeBlock>}
        {e.fields.length > 0 && <KeyValueList layout="grid" items={e.fields.map((f) => ({ key: f.key, label: f.label, value: f.value }))} />}
      </div>
    ) : undefined,
    detailLabel: `Details for ${where || e.kind}`,
    defaultOpen: open,
  };
}

/**
 * Activity (A1): everything the daemon kept in memory, newest first, filtered by facet and search.
 * It has no actions. The filters live in the query string so a view can be linked.
 *
 * @useWhen the dashboard's `/activity` view, inside the Dashboard shell.
 * @states busy, filtered, row open, run phases open, search with no match, empty, stale
 */
export function Activity({ state, now, stale = false, initialQuery, openFirstMessage = false, openFirstRun = false, loadRun = getRun }: ActivityProps) {
  const syncUrl = initialQuery === undefined;
  const [filters, setFilters] = useState<Filters>(() => parseQuery(initialQuery ?? readLocationQuery()));
  useEffect(() => {
    if (syncUrl) writeLocationQuery(toQuery(filters));
  }, [syncUrl, filters]);

  const events = useMemo(() => buildEvents(state), [state]);
  const facets = useMemo(() => buildFacets(events), [events]);
  const shown = useMemo(() => applyFilters(events, filters), [events, filters]);
  const filtered = isFiltered(filters);
  const opensFirst = (e: ActivityEvent) => (openFirstMessage && e.message !== undefined) || (openFirstRun && e.kind === 'outcome' && e.runner !== undefined);
  const firstOpen = openFirstMessage || openFirstRun ? shown.find(opensFirst)?.key : undefined;
  const rows = useMemo(() => shown.map((e) => toRow(e, now, e.key === firstOpen, loadRun)), [shown, now, firstOpen, loadRun]);

  const count = filtered ? `${shown.length} of ${events.length} events` : `${events.length} ${events.length === 1 ? 'event' : 'events'}`;
  // The daemon lists a bounded number of refused jobs and counts the rest; they belong to the refused
  // events, so a filter that leaves those out leaves this line out too.
  const kinds = filters.facets.kind;
  const others = filters.q.trim() !== '' || filters.facets.result.length + filters.facets.repo.length + filters.facets.actor.length > 0;
  const unlisted = !others && (kinds.length === 0 || kinds.includes('refused')) ? (state.refused_queued_overflow ?? 0) : 0;
  const empty = events.length === 0 ? 'Nothing has happened since this daemon started.' : 'No events match.';

  return (
    <div className="flex w-full min-w-0 flex-col gap-xl">
      <PageHeader
        title="Activity"
        live={stale ? 'stale' : true}
        meta={count}
        actions={
          <div className="w-rail">
            <Input
              type="search"
              leadingIcon={Search}
              aria-label="Search by repo, runner or job"
              placeholder="Search runner, job id, or repo…"
              value={filters.q}
              onValueChange={(q) => setFilters((f) => ({ ...f, q }))}
            />
          </div>
        }
      />
      <div className="flex w-full items-start gap-xl">
        {events.length > 0 && (
          <aside className="w-facets shrink-0">
            <FacetList
              groups={facets}
              selected={filters.facets}
              onChange={(id, values) => setFilters((f) => ({ ...f, facets: { ...f.facets, [id as FacetId]: values } }))}
            />
          </aside>
        )}
        <section className="min-w-0 flex-1 rounded-lg border border-border bg-surface py-md">
          <DataTable columns={COLUMNS} rows={rows} caption="Activity" empty={empty} />
          {unlisted > 0 && (
            <p className="border-t border-border-soft px-lg py-md text-small text-amber">
              +{unlisted} more refused {unlisted === 1 ? 'job' : 'jobs'} the daemon did not list
            </p>
          )}
          <div className="flex items-center gap-md border-t border-border-soft px-lg pt-md text-caption text-faint">
            <span>
              {events.length === 0
                ? 'Only events since this daemon started are kept.'
                : `Showing ${shown.length} of ${events.length} events — the last ${events.length} events this daemon kept since it started.`}
            </span>
            {filtered && (
              <span className="ml-auto">
                <Button variant="link" size="sm" onClick={() => setFilters(emptyFilters())}>
                  Clear filters
                </Button>
              </span>
            )}
          </div>
        </section>
      </div>
    </div>
  );
}
