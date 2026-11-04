import { useState } from 'react';
import { AttentionCard } from '../blocks/AttentionCard';
import { DismissedList } from '../blocks/DismissedList';
import { warningAlarms } from '../dashboard/alarms';
import { DISMISS, RESTORE_ALL, canDismiss, plainText, rowIdent, splitDismissed } from '../popover/attention';
import { ClassDetail } from '../blocks/ClassDetail';
import { BudgetRail } from '../blocks/BudgetRail';
import { DataTable, type DataTableColumn, type DataTableRow } from '../blocks/DataTable';
import { KeyValueList } from '../blocks/KeyValueList';
import { PageHeader } from '../blocks/PageHeader';
import { Panel } from '../blocks/Panel';
import { SampleBars, SegmentedControl, StatusBadge, StatusDot } from '../kit';
import { fmtAgo } from '../popover/format';
import {
  classViews,
  filterClasses,
  filterCounts,
  gibText,
  limitRows,
  parkedGates,
  peakText,
  unpinRequest,
  pinRequest,
  type ClassFilter,
  type ClassView,
} from '../dashboard/memory';
// One rail model for every view that draws the budget.
import { budgetJobs } from '../dashboard/overview';
import type { ViewProps } from '../dashboard/screen';

export interface MemoryProps extends ViewProps {
  /** The last poll failed: the header says Stale and the numbers are the last good ones. */
  stale?: boolean;
  /** The query string the view starts from, `?class=acme/web::1::build`; the page's own when omitted (stories pass one). */
  initialQuery?: string;
  /** The filter the table starts on. */
  initialFilter?: ClassFilter;
  /** A pin save already in flight, as a story shows it. */
  initialPending?: boolean;
  /** A failed pin save's text, as a story shows it. */
  initialError?: string;
  /** Whether the dismissed warnings show at first (a story's open state). */
  initialDismissedOpen?: boolean;
}

const COLUMNS: DataTableColumn[] = [
  { key: 'dot', header: '', width: 'tight' },
  { key: 'class', header: 'Class', width: 'fill' },
  { key: 'runner', header: 'Runner', width: 'tight', align: 'end', mono: true },
  { key: 'dind', header: 'Dind', width: 'tight', align: 'end', mono: true },
  { key: 'peak', header: 'Measured peak', width: 'tight', align: 'end', mono: true },
  { key: 'samples', header: 'Samples', width: 'tight' },
  { key: 'status', header: 'Status', width: 'tight' },
];

const GATE_COLUMNS: DataTableColumn[] = [
  { key: 'gate', header: 'Gate', width: 'fill' },
  { key: 'now', header: 'Parked now', width: 'tight', align: 'end', mono: true },
  { key: 'arrivals', header: 'Arrivals', width: 'tight', align: 'end', mono: true },
  { key: 'time', header: 'Time parked', width: 'tight', align: 'end', mono: true },
  { key: 'bound', header: 'Bound', width: 'tight', align: 'end', mono: true },
];

/** The `?class=` query: reads the selected class key, writes it back without adding history. */
function useSelectedClass(initialQuery: string | undefined): [string | undefined, (key: string | undefined) => void] {
  const read = () => {
    const search = initialQuery ?? (typeof window === 'undefined' ? '' : window.location.search);
    return new URLSearchParams(search).get('class') ?? undefined;
  };
  const [selected, setSelected] = useState(read);
  const select = (key: string | undefined) => {
    setSelected(key);
    if (initialQuery !== undefined || typeof window === 'undefined') return;
    const url = new URL(window.location.href);
    if (key === undefined) url.searchParams.delete('class');
    else url.searchParams.set('class', key);
    window.history.replaceState(null, '', url);
  };
  return [selected, select];
}

function rowOf(c: ClassView): DataTableRow {
  const dot = c.inFlight ? <StatusDot tone="green" label="Running now" /> : <StatusDot tone={c.warned ? 'amber' : 'neutral'} />;
  return {
    key: c.key,
    tone: c.warned ? 'amber' : undefined,
    cells: {
      dot,
      class: (
        <>
          <span className="block truncate font-mono">{c.name}</span>
          {c.note && <span className={`block truncate text-caption ${c.warned || c.bimodal ? 'text-amber' : 'text-faint'}`}>{c.note}</span>}
        </>
      ),
      runner: `${gibText(c.runnerMib)}G`,
      dind: `${gibText(c.dindMib)}G`,
      peak: <span className={c.overCap ? 'text-red' : undefined}>{c.peakMib === null ? peakText(c) : `${peakText(c)}G`}</span>,
      samples: (
        <span className="flex flex-col">
          <span className="flex items-center gap-xs">
            {c.samples}
            <SampleBars count={c.samples} />
          </span>
          {c.lastRanAgeS !== null && <span className="text-caption font-normal text-faint">{fmtAgo(c.lastRanAgeS)} ago</span>}
        </span>
      ),
      status: c.statusLabel ? <StatusBadge tone={c.statusTone}>{c.statusLabel}</StatusBadge> : null,
    },
  };
}

const READING = [
  ['Runner + dind is what gets admitted against.', ' A pinned cap came from a person; a learned one adjusts on its own.'],
  ['Learning means fewer than 15 samples, still finding its footing.', ' The tick strip and the age of the last sample say how much to trust the number next to them.'],
  ['Warned means faber measured over half the budget and stopped adjusting automatically.', ' Pin it to move on.'],
  ['A split peak like 3.1 to 8.2 GiB means the class runs hot and cold.', ' One cap cannot cover both: open it to see why.'],
] as const;

/**
 * Memory (/memory, M1): the budget, then every job class with its caps and measured peaks, the
 * limits behind admission and what parked jobs. A selected class opens beside the table, where
 * its cap can be pinned (docs/design/dashboard/screens.md § 4).
 *
 * @useWhen the dashboard's `/memory` view, inside the Dashboard shell.
 * Its warnings are the Overview's `warning:` alarms, keyed by class: each carries Dismiss, and a
 * dismissed one folds into "N dismissed" under them (ADR-0064).
 *
 * @states busy, class selected, pinned class selected, pin saving, pin failed, empty, stale, warning dismissable, warnings dismissed
 */
export function Memory({
  state,
  now,
  run,
  stale = false,
  initialQuery,
  initialFilter = 'all',
  initialPending = false,
  initialError,
  initialDismissedOpen = false,
}: MemoryProps) {
  const [filter, setFilter] = useState<ClassFilter>(initialFilter);
  const [selectedKey, select] = useSelectedClass(initialQuery);
  const [pending, setPending] = useState(initialPending);
  const [error, setError] = useState<string | undefined>(initialError);

  const budget = state.budget ?? {};
  const inflight = state.inflight ?? [];
  const classes = classViews(state, now);
  const counts = filterCounts(classes);
  const rows = filterClasses(classes, filter).map(rowOf);
  const selected = classes.find((c) => c.key === selectedKey);
  const limits = limitRows(budget);
  const gates = parkedGates(budget);
  const { active: warnings, dismissed } = splitDismissed(warningAlarms(state), state.attention);
  const [dismissedOpen, setDismissedOpen] = useState(initialDismissedOpen);
  // Each warning's action in flight and its error, by key; the dismissed list's under `dismissed`.
  const [acts, setActs] = useState<Record<string, { pending?: string; error?: string }>>({});
  const act = async (key: string, label: string, action: Parameters<typeof run>[0]) => {
    setActs((a) => ({ ...a, [key]: { pending: label } }));
    try {
      await run(action);
      setActs((a) => ({ ...a, [key]: {} }));
    } catch (e) {
      setActs((a) => ({ ...a, [key]: { error: e instanceof Error ? e.message : String(e) } }));
    }
  };

  const choose = (key: string | undefined) => {
    setError(undefined);
    select(key);
  };
  const send = async (req: NonNullable<ReturnType<typeof pinRequest>>) => {
    setPending(true);
    setError(undefined);
    try {
      await run({ cmd: 'pin', req });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPending(false);
    }
  };

  const total = (budget.budget_mib ?? 0) + (budget.reserve_mib ?? 0);
  const parked = Object.values(budget.gates ?? {}).reduce((n, g) => n + (g?.parked_now ?? 0), 0);

  return (
    <div className="flex w-full min-w-0 flex-col gap-2xl">
      <PageHeader title="Memory" live={stale ? 'stale' : true} actions={<p className="text-caption text-faint">Learned caps adjust continuously. Pins are the only manual override.</p>} />

      {(warnings.length > 0 || dismissed.length > 0) && (
        <div className="flex flex-col gap-lg">
          {warnings.map((w) => (
            <AttentionCard
              key={w.key}
              tone="amber"
              title="Needs attention"
              body={plainText(w.parts)}
              pending={acts[w.key]?.pending}
              error={acts[w.key]?.error}
              onDismiss={canDismiss(w) ? () => void act(w.key, DISMISS, { cmd: 'dismiss_attention', key: w.key, ident: rowIdent(w) }) : undefined}
            />
          ))}
          {dismissed.length > 0 && (
            <DismissedList
              card
              rows={dismissed}
              open={dismissedOpen}
              onOpenChange={setDismissedOpen}
              pending={acts.dismissed?.pending}
              error={acts.dismissed?.error}
              onRestore={(key) => void act('dismissed', key ?? RESTORE_ALL, key ? { cmd: 'restore_attention', key } : { cmd: 'restore_attention' })}
            />
          )}
        </div>
      )}

      {total > 0 && (
        <BudgetRail
          budgetMib={total}
          reserveMib={budget.reserve_mib ?? 0}
          jobs={budgetJobs(state)}
          admitted={budget.docker_inflight ?? inflight.filter((j) => j.admitted).length}
          parked={parked}
          safetyMargin={budget.margin}
        />
      )}

      <div className="flex flex-col gap-xl">
        <Panel
          title="Classes"
          bare
          action={
            classes.length > 0 && (
              <SegmentedControl
                label="Filter classes"
                value={filter}
                onValueChange={(v) => setFilter(v as ClassFilter)}
                items={[
                  { value: 'all', label: 'All', count: counts.all },
                  { value: 'pinned', label: 'Pinned', count: counts.pinned },
                  { value: 'warned', label: 'Warned', count: counts.warned, tone: counts.warned > 0 ? 'amber' : undefined },
                  { value: 'bimodal', label: 'Bimodal', count: counts.bimodal },
                ]}
              />
            )
          }
        >
          {classes.length === 0 ? (
            <p className="rounded-lg border border-border bg-surface px-xl py-lg text-small text-muted">No job classes yet: faber learns a class from its first run.</p>
          ) : (
            <div className="flex flex-col items-start gap-xl wide:flex-row">
              <div className="flex w-full min-w-0 flex-1 flex-col gap-xl">
                <div className="overflow-x-auto rounded-lg border border-border bg-surface py-lg">
                  <DataTable
                    caption="Job classes"
                    empty="No classes match this filter."
                    columns={COLUMNS}
                    rows={rows}
                    selectedKey={selected?.key}
                    onSelect={(key) => choose(key === selected?.key ? undefined : key)}
                  />
                </div>
                <Panel title="Reading this table">
                  <div className="flex flex-col gap-sm px-xl py-lg text-small text-muted">
                    {READING.map(([lead, rest]) => (
                      <p key={lead}>
                        <span className="text-fg">{lead}</span>
                        {rest}
                      </p>
                    ))}
                  </div>
                </Panel>
              </div>
              {selected && (
                <div className="w-full shrink-0 wide:w-detail">
                  <ClassDetail
                    key={selected.key}
                    cls={selected}
                    pending={pending}
                    error={error}
                    onClose={() => choose(undefined)}
                    onPin={(runnerMib, dindMib) => void send({ class: selected.key, pin: true, runner_mib: runnerMib, dind_mib: dindMib })}
                    onUnpin={() => void send(unpinRequest(selected.key))}
                  />
                </div>
              )}
            </div>
          )}
        </Panel>
      </div>

      {limits.length > 0 && (
        <Panel title="Limits">
          <div className="px-xl py-md">
            <KeyValueList items={limits.map((r) => ({ key: r.key, label: r.label, value: r.value, tone: r.tone, note: r.note }))} />
          </div>
        </Panel>
      )}

      {gates.length > 0 && (
        <Panel title="What parked jobs">
          <div className="py-lg">
            <DataTable
              caption="Gates that parked jobs"
              empty="No jobs have been parked."
              columns={GATE_COLUMNS}
              rows={gates.map((g) => ({
                key: g.gate,
                tone: g.parkedNow > 0 ? 'amber' : undefined,
                cells: { gate: g.label, now: g.parkedNow, arrivals: g.arrivals, time: g.parkedFor, bound: g.binding },
              }))}
            />
          </div>
        </Panel>
      )}
    </div>
  );
}
