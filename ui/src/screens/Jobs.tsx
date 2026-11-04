import { useEffect, useState } from 'react';
import { DataTable, type DataTableColumn, type DataTableRow } from '../blocks/DataTable';
import { KeyValueList, type KeyValueItem } from '../blocks/KeyValueList';
import { PageHeader } from '../blocks/PageHeader';
import { Pipe } from '../blocks/Pipe';
import { Panel } from '../blocks/Panel';
import {
  classSegments,
  fmtBytes,
  fmtCores,
  fmtMs,
  fmtShare,
  fmtSpread,
  getJobs,
  NOT_MEASURED,
  PHASE_LABEL,
  WINDOWS,
  type JobClass,
  type JobsView,
  type WindowDays,
} from '../dashboard/jobs';
import { gateName } from '../dashboard/memory';
import { PhaseBar, SegmentedControl, StatusBadge } from '../kit';

export interface JobsProps {
  /** The last state poll is old or failed: the header says "Stale". */
  stale?: boolean;
  /** Reads the view for a window, in days. The daemon's `/api/jobs` when omitted; stories pass fixtures. */
  load?: (days: number) => Promise<JobsView>;
  /** The window the view starts on. Default 7 days. */
  initialDays?: WindowDays;
}

/**
 * The parked share at which a class's figure turns amber: a quarter of its wall time spent waiting
 * for capacity. A starting value (owner, 2026-10-30), to revisit once real distributions show.
 */
const PARKED_ATTENTION_SHARE = 0.25;

/** The daemon's profiling threshold, for the copy before the first answer (src/jobs_view.rs). */
const PROFILING_RUNS = 20;

type Load = { status: 'ready'; view: JobsView } | { status: 'failed'; error: string };

const COLUMNS: DataTableColumn[] = [
  { key: 'class', header: 'Class' },
  { key: 'runs', header: 'Runs', width: 'tight', align: 'end', mono: true },
  { key: 'wall', header: 'Wall time', width: 'tight', align: 'end', mono: true },
  { key: 'where', header: 'Where the time went (medians)', width: 'fill' },
  { key: 'parked', header: 'Parked', width: 'tight', align: 'end', mono: true },
  { key: 'cpu', header: 'CPU used / reserved', width: 'tight', align: 'end', mono: true },
  { key: 'throttled', header: 'Throttled', width: 'tight', align: 'end', mono: true },
  { key: 'io', header: 'Bytes moved', width: 'tight', align: 'end', mono: true },
];

const faint = (text: string) => <span className="text-faint">{text}</span>;

/** A lower bound from the last sample: "≥ 1.2 GB", or "not measured". A zero is exactly zero. */
const atLeast = (value: number | null, fmt: (v: number | null) => string) => {
  if (value === null) return faint(NOT_MEASURED);
  return value === 0 ? fmt(value) : `≥ ${fmt(value)}`;
};

function cpuText(c: JobClass) {
  if (c.cpu_used_mcpu.median === null) return faint(NOT_MEASURED);
  const reserved = c.cpu_reserved_mcpu.median;
  return `${fmtCores(c.cpu_used_mcpu.median)} / ${reserved === null ? NOT_MEASURED : fmtCores(reserved)}`;
}

function parkedText(c: JobClass) {
  if (c.parked_share === null) return faint(NOT_MEASURED);
  return (
    <span className="flex flex-col items-end">
      <span className={c.parked_share >= PARKED_ATTENTION_SHARE ? 'text-amber' : undefined}>{fmtShare(c.parked_share)}</span>
      {c.top_gate && <span className="text-caption font-sans text-faint">{gateName(c.top_gate.gate)}</span>}
    </span>
  );
}

function detailItems(c: JobClass): KeyValueItem[] {
  const skewed = c.phases.filter((p) => p.skewed_runs > 0).map((p) => `${PHASE_LABEL[p.phase]} in ${p.skewed_runs} ${p.skewed_runs === 1 ? 'run' : 'runs'}`);
  const items: KeyValueItem[] = [
    { key: 'wall', label: 'Wall time', value: fmtSpread(c.wall, fmtMs), note: 'GitHub queued it to faber’s cleanup' },
    {
      key: 'parked',
      label: 'Parked share',
      value: fmtShare(c.parked_share),
      note: c.top_gate ? `bound most often by ${gateName(c.top_gate.gate)} (${c.top_gate.runs} of ${c.runs} runs)` : 'no run parked at a gate',
    },
    { key: 'cpu-used', label: 'CPU used, average', value: fmtSpread(c.cpu_used_mcpu, fmtCores), note: 'runner and dind together, over the sampled run' },
    { key: 'cpu-reserved', label: 'CPU reserved', value: fmtSpread(c.cpu_reserved_mcpu, fmtCores), note: 'the share admission debited' },
    { key: 'throttled', label: 'Lost to throttling', value: fmtSpread(c.throttled_ms, fmtMs), note: 'a lower bound: as of the last sample' },
    { key: 'io', label: 'Bytes moved', value: fmtSpread(c.io_bytes, fmtBytes), note: 'block and network, a lower bound: as of the last sample' },
  ];
  if (c.unmeasured_resource_runs > 0)
    items.push({ key: 'macos', label: 'Resources not measured', value: `${c.unmeasured_resource_runs} of ${c.runs} runs`, note: 'macOS VMs have no cgroups: phases and steps only' });
  if (skewed.length > 0)
    items.push({ key: 'skewed', label: 'Clocks disagreed', value: skewed.join(', '), tone: 'amber', note: 'read as zero: the host’s clock and GitHub’s differ' });
  return items;
}

function rowOf(c: JobClass, threshold: number): DataTableRow {
  const p90: Record<string, string> = Object.fromEntries(c.phases.filter((p) => p.p90 !== null).map((p) => [p.phase, fmtMs(p.p90)]));
  for (const [key, s] of [['job-setup', c.split.setup], ['job-steps', c.split.steps], ['job-post', c.split.teardown]] as const)
    if (s.p90 !== null) p90[key] = fmtMs(s.p90);
  return {
    key: c.key,
    cells: {
      // The job leads in the UI face; where it runs is the quiet mono line under it (owner, 2026-10-30).
      class: (
        <span className="flex min-w-0 flex-col">
          <span className="flex min-w-0 items-center gap-sm">
            <span className="truncate text-fg">{c.job}</span>
            {c.profiling && <StatusBadge tone="neutral">Profiling</StatusBadge>}
          </span>
          <span className="truncate font-mono text-caption text-muted">
            {c.repo}
            {c.workflow !== null && (
              <>
                <Pipe />
                {c.workflow}
              </>
            )}
          </span>
          {c.profiling && <span className="text-caption text-faint">fewer than {threshold} runs: not ranked</span>}
        </span>
      ),
      runs: c.runs,
      wall: fmtSpread(c.wall, fmtMs),
      where: <PhaseBar compact segments={classSegments(c)} format={fmtMs} />,
      parked: parkedText(c),
      cpu: cpuText(c),
      throttled: atLeast(c.throttled_ms.median, fmtMs),
      io: atLeast(c.io_bytes.median, fmtBytes),
    },
    detail: (
      <div className="flex flex-col gap-lg pt-xs">
        <PhaseBar segments={classSegments(c)} format={fmtMs} secondary={p90} />
        <KeyValueList layout="grid" items={detailItems(c)} />
      </div>
    ),
    detailLabel: `Details for ${c.label}`,
  };
}

/**
 * Jobs (/jobs, ADR-0056 § 5): one row per job class over a window, saying where its runs' time
 * went (each phase's median, the job split into setup, steps and post steps), whether they waited
 * or ran (the parked share and the gate that bound most often), and whether they were starved or
 * idle (CPU used against reserved, time lost to throttling, bytes moved). A class under the profiling threshold is
 * shown as profiling and sorts last. Anything not measured says so; it is never drawn as zero.
 *
 * @useWhen the dashboard's `/jobs` view, inside the Dashboard shell.
 * @states busy, loading, empty, failed, profiling class, macOS class, stale
 */
export function Jobs({ stale = false, load = getJobs, initialDays = 7 }: JobsProps) {
  const [days, setDays] = useState<WindowDays>(initialDays);
  // The answer for the window it was read for: a window with none yet is loading.
  const [answer, setAnswer] = useState<{ days: number; read: Load } | null>(null);

  useEffect(() => {
    let live = true;
    load(days).then(
      (view) => live && setAnswer({ days, read: { status: 'ready', view } }),
      (e: unknown) => live && setAnswer({ days, read: { status: 'failed', error: e instanceof Error ? e.message : String(e) } }),
    );
    return () => {
      live = false;
    };
  }, [load, days]);

  const read: Load | { status: 'loading' } = answer?.days === days ? answer.read : { status: 'loading' };
  const view = read.status === 'ready' ? read.view : null;
  const ranked = view?.classes.filter((c) => !c.profiling).length ?? 0;
  const meta = view ? `${view.classes.length} ${view.classes.length === 1 ? 'class' : 'classes'}, ${ranked} ranked in the last ${view.window_days} days` : undefined;

  return (
    <div className="flex w-full min-w-0 flex-col gap-xl">
      <PageHeader
        title="Jobs"
        live={stale ? 'stale' : true}
        meta={meta}
        actions={
          <SegmentedControl
            label="Window"
            value={String(days)}
            onValueChange={(v) => setDays(Number(v) as WindowDays)}
            items={WINDOWS.map((d) => ({ value: String(d), label: `${d} days` }))}
          />
        }
      />
      <p className="text-small text-muted">
        Ranked by total wall time. Open a class for each phase’s p90 and its resources. Throttled time and bytes are lower bounds: the last
        sample is taken before a container exits.
      </p>
      <section className="overflow-x-auto rounded-lg border border-border bg-surface py-md">
        {read.status === 'loading' ? (
          <p role="status" className="px-lg py-md text-small text-muted">
            Reading the audit log…
          </p>
        ) : read.status === 'failed' ? (
          <p role="alert" className="px-lg py-md text-small text-red">
            Could not read the jobs: {read.error}
          </p>
        ) : (
          <DataTable
            caption="Job classes by where their time went"
            columns={COLUMNS}
            rows={read.view.classes.map((c) => rowOf(c, read.view.profiling_runs))}
            empty={`No run finished in the last ${read.view.window_days} days.`}
          />
        )}
      </section>
      <Panel title="Reading this table">
        <ul className="flex flex-col gap-sm px-xl py-lg text-small text-muted">
          <li>
            <span className="text-fg">Hatched amber is waiting for capacity, grey is faber’s and GitHub’s overhead,</span> the accent is the job’s own setup and
            strong ink its steps.
          </li>
          <li>
            <span className="text-fg">Setup is a fixed list:</span> GitHub’s set up job, checkout, setup and cache actions, and installs GitHub named from
            their command. An install inside a step named test counts as a job step.
          </li>
          <li>
            <span className="text-fg">A class under {view?.profiling_runs ?? PROFILING_RUNS} runs is profiling.</span> Its numbers show, but two runs must not rank the levers.
          </li>
        </ul>
      </Panel>
    </div>
  );
}
