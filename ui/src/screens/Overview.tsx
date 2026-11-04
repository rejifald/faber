import { useState, type ReactNode } from 'react';
import { AttentionCard, type AttentionCardAction } from '../blocks/AttentionCard';
import { BudgetRail } from '../blocks/BudgetRail';
import { DataTable, type DataTableRow } from '../blocks/DataTable';
import { KeyValueList } from '../blocks/KeyValueList';
import { PageHeader } from '../blocks/PageHeader';
import { Panel } from '../blocks/Panel';
import { Pipe } from '../blocks/Pipe';
import { StatTile } from '../blocks/StatTile';
import { CodeBlock } from '../blocks/CodeBlock';
import { DismissedList } from '../blocks/DismissedList';
import { SegmentedControl, StatusBadge, StatusDot, TextLink } from '../kit';
import { alarmAction, type DashboardAction, type ViewProps } from '../dashboard/screen';
import { activeAlarms } from '../dashboard/alarms';
import {
  ACTIVITY_FILTERS,
  activityCounts,
  activityRows,
  activityWhen,
  alarmCard,
  budgetJobs,
  filterActivity,
  hostFacts,
  inflightRows,
  jobCounts,
  learnedCaps,
  statTiles,
  type ActivityFilter,
} from '../dashboard/overview';
import { DISMISS, RESTORE_ALL, canDismiss, rowIdent, type Part, type RowAction } from '../popover/attention';
import { gib } from '../popover/limits';
import { plural } from '../popover/format';

export interface OverviewProps extends ViewProps {
  /** The last good poll is older than the shell's staleness bound (S10): the live dot turns amber "Stale". */
  stale?: boolean;
  /** Whether the dismissed alarms show when the view first renders (a story's open state). */
  defaultDismissedOpen?: boolean;
}

const FILTER_LABEL: Record<ActivityFilter, string> = { all: 'All', delivered: 'Delivered', faults: 'Faults', refused: 'Refused', retried: 'Retried' };

/** One run of an alarm's words, set as text: the head bold, a value mono, a pipe between independent values. */
function part(p: Part, i: number): ReactNode {
  if (typeof p === 'string') return p;
  if ('strong' in p) return <strong key={i} className="font-bold text-fg-strong">{p.strong}</strong>;
  if ('mono' in p) return <span key={i} className="font-mono text-caption">{p.mono}</span>;
  return <Pipe key={i} />;
}

const reason = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Where the dismissed list's Restore tracks its pending action and error, beside the cards'. */
const DISMISSED = ' dismissed';

/**
 * The dashboard's home view (screens.md § 4): is the host healthy, what is it doing, what needs
 * me. The memory budget, four since-start tiles, the jobs in
 * flight and the latest activity, and a rail with the alarms, the host and the learned caps.
 *
 * Host actions (Reconcile, Pause, Hold, Drain) live in Maintenance (S13); an alarm card keeps the one fix it names.
 *
 * @useWhen the dashboard's `/` view. A pure function of the state it is handed; actions go
 * through `run`.
 * @states busy, healthy, fresh host (empty panels), stale, alarm action failed, alarms dismissed, dismissed open, trust changes acknowledged
 */
export function Overview({ state, now, run, stale = false, defaultDismissedOpen = false }: OverviewProps) {
  const [filter, setFilter] = useState<ActivityFilter>('all');
  const [dismissedOpen, setDismissedOpen] = useState(defaultDismissedOpen);
  const [cards, setCards] = useState<Record<string, { pending?: string; error?: string }>>({});

  // The action whose confirmation is open, by alarm key (a risk-accepting action asks first, #385).
  const [asking, setAsking] = useState<{ key: string; label: string; action: DashboardAction; confirm: NonNullable<RowAction['confirm']> } | null>(null);

  const budget = state.budget;

  async function alarm(key: string, label: string, action: DashboardAction) {
    setCards((c) => ({ ...c, [key]: { pending: label } }));
    try {
      await run(action);
      setCards((c) => ({ ...c, [key]: {} }));
    } catch (e) {
      setCards((c) => ({ ...c, [key]: { error: reason(e) } }));
    }
  }

  const tiles = statTiles(state);
  const inflight = inflightRows(state);
  const counts = jobCounts(state);
  const rows = activityRows(state);
  const tally = activityCounts(rows);
  const shown = filterActivity(rows, filter);
  // ADR-0064: dismissed alarms leave the list for the "N dismissed" line under it.
  const { active: alarms, dismissed } = activeAlarms(state, now);
  const restoring = cards[DISMISSED];
  const caps = learnedCaps(state);
  const host = hostFacts(state);

  const inflightTable: DataTableRow[] = inflight.map((r) => ({
    key: r.key,
    tone: r.tone,
    bar: r.tone,
    cells: {
      repo: (
        <span className="inline-flex items-center gap-sm">
          <StatusDot tone={r.dot} pulse={r.pulse} />
          <span className="font-sans font-semibold text-fg-strong">{r.repo}</span>
          {r.ref && <span className="text-small font-normal text-faint">{r.ref}</span>}
        </span>
      ),
      job: r.job,
      step: <span className={r.tone === 'red' ? 'text-red' : r.tone === 'amber' ? 'text-amber' : 'text-muted'}>{r.step}</span>,
      memory: r.mib === null ? '—' : `${gib(r.mib)} GiB`,
      elapsed: r.elapsed,
    },
  }));

  const activityTable: DataTableRow[] = shown.map((r) => ({
    key: r.key,
    cells: {
      when: <span className="text-muted">{activityWhen(r.ts_ms, now)}</span>,
      result: <StatusBadge tone={r.tone}>{r.result}</StatusBadge>,
      repo: <span className="font-sans font-semibold text-fg-strong">{r.repo}</span>,
      job: r.job,
      detail: <span className={r.detailTone ? 'text-red' : 'text-muted'}>{r.detail}</span>,
      took: r.took,
    },
    ...(r.cause
      ? {
          detailLabel: `Details for ${r.repo} ${r.job}`,
          detail: (
            <div className="flex flex-col gap-sm">
              <CodeBlock>{r.cause}</CodeBlock>
              {(r.runner || r.attempt) && (
                <p className="font-mono text-caption text-faint">
                  {r.runner}
                  {r.runner && r.attempt ? <Pipe /> : null}
                  {r.attempt ? `attempt ${r.attempt}` : null}
                </p>
              )}
            </div>
          ),
        }
      : {}),
  }));

  return (
    <div className="flex w-full min-w-0 flex-col gap-2xl">
      <PageHeader title="Overview" live={stale ? 'stale' : true} />

      {budget?.budget_mib ? (
        <BudgetRail
          budgetMib={budget.budget_mib}
          reserveMib={budget.reserve_mib ?? 0}
          jobs={budgetJobs(state)}
          admitted={counts.admitted}
          parked={counts.parked}
          safetyMargin={budget.margin}
        />
      ) : (
        <Panel title="Memory budget">
          <p className="px-lg py-md text-small text-muted">faber has not measured the machine yet. The budget appears with the first poll of Docker.</p>
        </Panel>
      )}

      <div className="grid grid-cols-2 gap-lg narrow:grid-cols-4">
        {tiles.map((t) => (
          <StatTile
            key={t.key}
            label={t.label}
            tone={t.tone}
            note={t.note}
            value={
              t.unit ? (
                <>
                  {t.value}
                  <span className="text-small font-normal text-muted">{t.unit}</span>
                </>
              ) : (
                t.value
              )
            }
          />
        ))}
      </div>

      <div className="flex flex-wrap items-start gap-2xl">
        <div className="flex min-w-0 flex-1 flex-col gap-2xl narrow:min-w-popover-wide">
          <Panel title="In flight" meta={inflight.length ? `${counts.admitted} admitted, ${counts.parked} parked` : undefined}>
            <DataTable
              caption="Jobs in flight"
              empty="Nothing is running. A job appears here the moment faber admits it."
              columns={[
                { key: 'repo', header: 'Repo' },
                { key: 'job', header: 'Job' },
                { key: 'step', header: 'Step', width: 'fill' },
                { key: 'memory', header: 'Memory', align: 'end', mono: true },
                { key: 'elapsed', header: 'Elapsed', align: 'end', mono: true },
              ]}
              rows={inflightTable}
            />
          </Panel>

          <Panel
            title="Latest activity"
            action={
              <SegmentedControl
                label="Filter activity"
                value={filter}
                onValueChange={(v) => setFilter(v as ActivityFilter)}
                items={ACTIVITY_FILTERS.map((f) => ({ value: f, label: FILTER_LABEL[f], count: tally[f], ...(f === 'faults' && tally.faults > 0 ? { tone: 'amber' as const } : {}) }))}
              />
            }
          >
            <DataTable
              caption="Latest activity"
              empty={
                rows.length === 0
                  ? 'No activity since faber started. Finished runs, refusals and retries appear here.'
                  : `Nothing ${FILTER_LABEL[filter].toLowerCase()} since faber started.`
              }
              columns={[
                { key: 'when', header: 'When', mono: true },
                { key: 'result', header: 'Result' },
                { key: 'repo', header: 'Repo' },
                { key: 'job', header: 'Job', mono: true },
                { key: 'detail', header: 'Detail', width: 'fill' },
                { key: 'took', header: 'Took', align: 'end', mono: true },
              ]}
              rows={activityTable}
            />
            <div className="border-t border-border-soft px-lg py-md">
              <TextLink href="/activity" arrow>{rows.length ? `All activity (${plural(rows.length, 'event')} held)` : 'All activity'}</TextLink>
            </div>
          </Panel>
        </div>

        <div className="flex w-rail max-w-full shrink-0 flex-col gap-lg">
          {alarms.length === 0 ? (
            <p className="flex items-center gap-sm rounded-lg border border-border bg-surface px-xl py-lg text-small text-muted">
              <StatusDot tone="green" />
              {dismissed.length ? 'Nothing else needs you' : 'Nothing needs you'}
            </p>
          ) : (
            alarms.map((a) => {
              const m = alarmCard(a);
              const st = cards[a.key];
              const actions = (a.actions || []).flatMap((x): AttentionCardAction[] => {
                const act = alarmAction(x.cmd, x.args);
                if (!act) return [];
                const onClick = x.confirm
                  ? () => setAsking({ key: a.key, label: x.label, action: act, confirm: x.confirm! })
                  : () => void alarm(a.key, x.label, act);
                return [{ label: x.label, primary: x.primary, onClick }];
              });
              const ask = asking?.key === a.key ? asking : null;
              return (
                <AttentionCard key={a.key} tone={m.tone} title={m.title} body={m.body.map(part)} actions={actions.length ? actions : undefined} pending={st?.pending} error={st?.error}
                  onDismiss={canDismiss(a) ? () => void alarm(a.key, DISMISS, { cmd: 'dismiss_attention', key: a.key, ident: rowIdent(a) }) : undefined}
                  details={a.details ? { label: a.details.label, lines: a.details.lines.map((l) => l.map(part)) } : undefined}
                  confirm={
                    ask
                      ? {
                          ...ask.confirm,
                          onCancel: () => setAsking(null),
                          onConfirm: () => {
                            setAsking(null);
                            void alarm(ask.key, ask.label, ask.action);
                          },
                        }
                      : undefined
                  }
                />
              );
            })
          )}
          {dismissed.length > 0 && (
            <DismissedList
              card
              rows={dismissed}
              open={dismissedOpen}
              onOpenChange={setDismissedOpen}
              pending={restoring?.pending}
              error={restoring?.error}
              onRestore={(key) => void alarm(DISMISSED, key ?? RESTORE_ALL, key ? { cmd: 'restore_attention', key } : { cmd: 'restore_attention' })}
            />
          )}

          <Panel title="Host">
            {host.length ? (
              <div className="px-xl py-md">
                <KeyValueList
                  items={host.map((h) => ({
                    key: h.key,
                    label: h.label,
                    tone: h.tone,
                    value: h.mono ? <span className="font-mono text-caption">{h.value}</span> : h.value,
                    note: h.note,
                  }))}
                />
              </div>
            ) : (
              <p className="px-xl py-md text-small text-muted">No host readings yet. They arrive with the first poll.</p>
            )}
          </Panel>

          <Panel title="Learned caps" meta={caps.length ? plural(caps.length, 'class', 'classes') : undefined}>
            {caps.length ? (
              <div className="flex flex-col gap-sm px-xl py-md">
                <KeyValueList
                  mono="label"
                  items={caps.slice(0, 6).map((c) => ({
                    key: c.key,
                    label: c.label,
                    value: (
                      <span className="inline-flex items-baseline gap-sm">
                        {c.badge ? (
                          <StatusBadge tone={c.badge === 'pinned' ? 'accent' : 'amber'}>{c.badge === 'pinned' ? 'Pinned' : 'Warned'}</StatusBadge>
                        ) : (
                          <span className="font-mono text-caption text-faint">{c.samples}</span>
                        )}
                        <span className="font-mono">{gib(c.mib)} GiB</span>
                      </span>
                    ),
                  }))}
                />
                <TextLink href="/memory" arrow>{caps.length > 6 ? `All ${caps.length} in Memory` : 'Open Memory'}</TextLink>
              </div>
            ) : (
              <div className="flex flex-col gap-sm px-xl py-md">
                <p className="text-small text-muted">No classes yet. faber learns a cap for each kind of job after it has run a few times.</p>
                <TextLink href="/memory" arrow>Open Memory</TextLink>
              </div>
            )}
          </Panel>
        </div>
      </div>
    </div>
  );
}
