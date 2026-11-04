import { Activity, CircleDashed, CircleQuestionMark, Hourglass, Moon, Power, type LucideIcon } from 'lucide-react';
import { Disclosure, Icon, type StatusTone } from '../kit';
import type { VmRowModel, VmStatus } from '../popover/vms';
import { FactList } from './FactList';
import { Pipe } from './Pipe';

/** The icon per status: its shape carries the state, so it reads without its colour. */
const ICON: Record<VmStatus, LucideIcon> = {
  serving: Activity,
  idle: Moon,
  settling: Hourglass,
  stopping: Power,
  unmanaged: CircleQuestionMark,
  unknown: CircleDashed,
};

const TONE: Record<StatusTone, string> = {
  green: 'text-green',
  amber: 'text-amber',
  red: 'text-red',
  neutral: 'text-faint',
  accent: 'text-accent',
};

export interface VmRowProps {
  /** The VM: its kind, what it serves, its status and tone, what it was given and its age. */
  vm: VmRowModel;
  /** The last known list while the supervisor is unreachable: neutral by the model, the age `—`. */
  dim?: boolean;
  /** Whether its detail shows. One VM is open at a time; the screen keeps which. */
  open: boolean;
  /** Called when the row is pressed. */
  onOpenChange: (open: boolean) => void;
}

/**
 * One virtual machine this host runs, as a row that opens in place like a gate: an icon for its
 * status (serving, idle, booting, stopping, unmanaged; a dashed ring while the list is stale), its kind, what it serves (`3 jobs`, a job
 * name; nothing otherwise, the icon says the rest) and its age. No slot number: it is the
 * supervisor's index and means nothing to a reader. Pressing it lists its state in words when it
 * is settling, stopping or unmanaged, what it was given (CPU, memory) and, for Linux, its free
 * disk. With none of those known it does not open. The job name is GitHub's text, set as a text node on one line.
 *
 * @useWhen the popover's Virtual machines section, one row per VM that is not stopped.
 * @states serving Linux, serving macOS, idle, booting, stopping, unmanaged, open, no spec, long job name, dim
 */
export function VmRow({ vm, dim = false, open, onOpenChange }: VmRowProps) {
  const summary = (
    <span className="flex items-center gap-sm">
      <span className={TONE[vm.tone]}>
        <Icon icon={ICON[vm.status]} size="sm" />
      </span>
      <span className="min-w-0 flex-1 truncate">
        <span className="font-medium text-fg">{vm.kind}</span>
        {vm.doing && (
          <span className="text-small text-faint">
            <Pipe />
            {vm.doing}
          </span>
        )}
      </span>
      <span className="w-age shrink-0 text-right font-mono text-caption whitespace-nowrap text-faint">{vm.age}</span>
    </span>
  );
  if (vm.facts.length === 0) {
    return (
      <div role="group" aria-label={vm.label} title={vm.doing || undefined} data-dim={dim || undefined} className="flex flex-col px-xl py-xs text-body">
        {summary}
      </div>
    );
  }
  return (
    <div data-dim={dim || undefined}>
      <Disclosure summary={summary} label={vm.label} title={vm.doing || undefined} open={open} onOpenChange={onOpenChange}>
        <FactList facts={vm.facts} />
      </Disclosure>
    </div>
  );
}
