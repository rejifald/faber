import { useEffect, useState } from 'react';
import { fmtBytes, fmtCores, fmtMs, NOT_MEASURED, runSegments, type RunFootprint } from '../dashboard/jobs';
import { PhaseBar } from '../kit';
import { KeyValueList } from './KeyValueList';

export interface RunPhasesProps {
  /** The runner whose run to show. */
  runner: string;
  /** Reads the run's footprint; null when no recorded run has that runner. */
  load: (runner: string) => Promise<RunFootprint | null>;
}

type Read = { status: 'loading' } | { status: 'ready'; run: RunFootprint | null } | { status: 'failed'; error: string };

/**
 * One run's phase bar and resources, read when its Activity row opens (ADR-0056 § 5). A run the
 * audit log has no footprint for says so in a line; what was not measured reads "not measured".
 *
 * @useWhen inside an Activity outcome row's detail. The per-class view is the Jobs screen.
 * @states loading, measured, partly measured, not recorded, failed
 */
export function RunPhases({ runner, load }: RunPhasesProps) {
  const [read, setRead] = useState<Read>({ status: 'loading' });
  useEffect(() => {
    let live = true;
    load(runner).then(
      (run) => live && setRead({ status: 'ready', run }),
      (e: unknown) => live && setRead({ status: 'failed', error: e instanceof Error ? e.message : String(e) }),
    );
    return () => {
      live = false;
    };
  }, [runner, load]);

  if (read.status === 'loading')
    return (
      <p role="status" className="text-small text-faint">
        Reading where this run’s time went…
      </p>
    );
  if (read.status === 'failed')
    return (
      <p role="alert" className="text-small text-red">Could not read this run’s phases: {read.error}
      </p>
    );
  const run = read.run;
  if (!run) return <p className="text-small text-faint">No phases recorded for this run: it finished before faber recorded them.</p>;
  const lowerBound = (value: number | null, fmt: (v: number | null) => string) => (value === null || value === 0 ? fmt(value) : `≥ ${fmt(value)}`);
  return (
    <div className="flex flex-col gap-md">
      <PhaseBar segments={runSegments(run)} format={fmtMs} />
      <KeyValueList
        layout="grid"
        items={[
          { key: 'cpu', label: 'CPU used / reserved', value: run.cpu_used_mcpu === null ? NOT_MEASURED : `${fmtCores(run.cpu_used_mcpu)} / ${fmtCores(run.cpu_reserved_mcpu)}` },
          { key: 'throttled', label: 'Lost to throttling', value: lowerBound(run.throttled_ms, fmtMs) },
          { key: 'io', label: 'Bytes moved', value: lowerBound(run.io_bytes, fmtBytes) },
          ...(run.split === null ? [{ key: 'steps', label: 'Steps', value: NOT_MEASURED, note: 'GitHub’s steps were not read for this run' }] : []),
        ]}
      />
    </div>
  );
}
