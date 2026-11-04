import type { PeerRowModel } from '../popover/peers';
import { PipStrip } from './PipStrip';

export interface PeerRowProps {
  /** The peer: its name, a pip per runner, whether it is stale, and when it was last seen. */
  peer: PeerRowModel;
  /** The tighter row of a popup list (HostCard's "Other hosts"): caption text, small padding. */
  dense?: boolean;
}

/**
 * Another host minting runners from the same pool: `peer <instance>`, its runners as the job
 * row's strip (a turning arc per busy runner, then a grey ring per idle one; the count and a bar
 * past six) and when it was last seen. A peer not seen for five minutes turns amber and says
 * `not seen` instead of pips. With no runner, `no runners`. Not pressable: there is nothing under
 * it to open. The label says the counts in words.
 *
 * @useWhen the popover, under the "Other hosts" label while busy; when idle, only while stale.
 * `dense` in the dashboard HostCard's popup.
 * @states fresh, all idle, stale, many runners, no runners, older daemon, dense
 */
export function PeerRow({ peer, dense = false }: PeerRowProps) {
  return (
    <div
      role="group"
      aria-label={peer.label}
      className={`flex items-center ${dense ? 'gap-lg px-sm py-2xs text-caption' : 'gap-sm px-xl py-xs text-body'}`}
    >
      <span className={`min-w-0 flex-1 truncate font-medium ${peer.stale ? 'text-amber' : 'text-muted'}`}>{peer.name}</span>
      {peer.pips.length > 0 ? (
        <PipStrip pips={peer.pips} />
      ) : (
        <span className={`shrink-0 ${dense ? '' : 'text-small'} whitespace-nowrap ${peer.stale ? 'text-amber' : 'text-faint'}`}>
          {peer.stale ? 'not seen' : 'no runners'}
        </span>
      )}
      <span className={`w-age shrink-0 text-right font-mono text-caption ${peer.stale ? 'text-amber' : 'text-faint'}`}>
        {peer.age}
      </span>
    </div>
  );
}
