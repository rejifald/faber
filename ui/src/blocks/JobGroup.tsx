import { Disclosure } from '../kit';
import type { Group, JobLine, JobStatus } from '../popover/groups';
import { Pipe } from './Pipe';
import { PipStrip } from './PipStrip';

const STATUS: Record<JobStatus['kind'], string> = {
  step: 'font-mono text-caption text-faint',
  waiting: 'text-small text-amber',
  frozen: 'text-small text-amber',
  fault: 'text-small text-red',
  done: 'text-small text-faint',
};

const NOTE = { amber: 'text-amber', red: 'text-red' };

function Line({ job }: { job: JobLine }) {
  return (
    <div title={job.title} className="flex items-baseline gap-sm py-3xs pr-xl pl-4xl text-body">
      <span className="min-w-0 flex-1 truncate text-muted">{job.name}</span>
      {job.status.text && <span className={`shrink-0 ${STATUS[job.status.kind]}`}>{job.status.text}</span>}
      <span className="w-age shrink-0 text-right font-mono text-caption whitespace-nowrap text-faint">{job.age}</span>
    </div>
  );
}

export interface JobGroupProps {
  /** The group: its name, pips, age, note and job lines. */
  group: Group;
  /** Whether its job lines show. One group is open at a time; the screen keeps which. */
  open: boolean;
  /** Called when the row is pressed. */
  onOpenChange: (open: boolean) => void;
}

/**
 * One PR (or one workflow of a repo) in the popover: name, a pip per job and the age, with a
 * line under it when a job is frozen or only runner faults are left. Waiting has no line: the clock
 * pips say it waits, and the open row's job lines say why. Pressing it
 * lists its jobs under it: name, step `i/n` (or why it waits, or how it finished), and elapsed.
 *
 * @useWhen the popover's job list, one per group, live groups first (popover/anatomy.md § 6).
 * @states running, mixed, all waiting, frozen, done, fault only, overflow bar, hover, open
 */
export function JobGroup({ group, open, onOpenChange }: JobGroupProps) {
  const summary = (
    <>
      <span className="flex items-center gap-sm">
        <span className="min-w-0 flex-1 truncate font-semibold text-fg-strong">
          {group.name}
          {group.workflow && (
            <>
              <Pipe />
              {group.workflow}
            </>
          )}
        </span>
        <PipStrip pips={group.pips} />
        <span className="w-age shrink-0 text-right font-mono text-caption whitespace-nowrap text-faint">
          {group.age}
        </span>
      </span>
      {group.note && (
        <span className={`truncate text-small ${NOTE[group.note.tone]}`}>
          {group.note.text}
        </span>
      )}
    </>
  );
  return (
    <Disclosure summary={summary} label={group.label} title={group.title} open={open} onOpenChange={onOpenChange}>
      {group.jobs.map((j) => (
        <Line key={j.key} job={j} />
      ))}
    </Disclosure>
  );
}
