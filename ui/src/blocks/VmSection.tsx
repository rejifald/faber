import type { VmRowModel } from '../popover/vms';
import { VmRow } from './VmRow';

export interface VmSectionProps {
  /** The VMs to show, Linux first and then macOS by slot, stopped ones left out. */
  rows: VmRowModel[];
  /** `as of 3m ago`, set while the list is stale, at the label's right. */
  asOf: string | null;
  /** The list is stale (the supervisor is unreachable): its rows are neutral. */
  dim: boolean;
  /** The key of the open row, if any. One VM is open at a time; the screen keeps which. */
  openKey?: string | null;
  /** Called with a row's key when it opens, `null` when it closes. */
  onOpenChange?: (key: string | null) => void;
}

/**
 * The label line and rows of the VMs this host runs. Each row opens in place to say what that VM
 * was given. The screen decides whether to render it at all: with no VM there is no section, and
 * an idle popover still shows it, since a running VM holds its memory.
 *
 * @useWhen the popover, under this host's gauges and above Other hosts, whenever a VM is up.
 * @states default, one row, a row open, stale, no sizes
 */
export function VmSection({ rows, asOf, dim, openKey = null, onOpenChange }: VmSectionProps) {
  return (
    <section aria-label="Virtual machines" className="flex flex-col gap-px py-xs">
      <p className="flex items-baseline justify-between gap-sm px-xl pt-2xs pb-3xs text-caption text-faint">
        <span className="font-semibold">Virtual machines</span>
        {asOf && <span className="font-mono whitespace-nowrap">{asOf}</span>}
      </p>
      {rows.map((r) => (
        <VmRow key={r.key} vm={r} dim={dim} open={openKey === r.key} onOpenChange={(open) => onOpenChange?.(open ? r.key : null)} />
      ))}
    </section>
  );
}
