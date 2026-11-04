import { ChevronDown } from 'lucide-react';
import { Icon, Popover, StatusDot, type StatusTone } from '../kit';
import type { PeerRowModel } from '../popover/peers';
import { PeerRow } from './PeerRow';

const TONE_LABEL: Record<StatusTone, string> = {
  green: 'Healthy',
  amber: 'Needs attention',
  red: 'Fault',
  neutral: 'Idle',
  accent: 'Busy',
};

export interface HostCardProps {
  /** This host's name. Set as text. */
  name: string;
  /** The host's state: green healthy, amber caution, red fault, neutral idle. The dot is named from it (Healthy, Needs attention, Fault, Idle, Busy). */
  tone: StatusTone;
  /** One phrase for the pool ("2 hosts, both healthy"). */
  summary: string;
  /** The other hosts in the pool, from `peerRows`. With none, the card has no popover. */
  peers: PeerRowModel[];
}

/**
 * Which host this dashboard is, whether it is healthy, and the other hosts in its pool. With peers
 * the whole card is a button that opens a read-only popover of them (name, runners, last seen; stale ones amber). It does
 * not switch hosts: the dashboard cannot reach another host's daemon.
 *
 * @useWhen under the brand in the dashboard sidebar. Not for a single peer row (use PeerRow).
 * @states default, no peers, with stale peer, popover open, hover, focus-visible
 */
export function HostCard({ name, tone, summary, peers }: HostCardProps) {
  const lines = (
    <>
      <StatusDot tone={tone} label={TONE_LABEL[tone]} />
      <span className="flex min-w-0 flex-1 flex-col text-left">
        <span className="truncate text-body font-semibold text-fg-strong">{name}</span>
        <span className="text-caption font-normal text-muted">{summary}</span>
      </span>
    </>
  );
  if (peers.length === 0) {
    return <div className="flex items-center gap-sm rounded-md border border-border bg-surface px-md py-sm">{lines}</div>;
  }
  return (
    <div className="flex flex-col overflow-hidden rounded-md border border-border bg-surface">
      <Popover
        triggerVariant="row"
        trigger={
          <>
            {lines}
            <span className="text-muted"><Icon icon={ChevronDown} /></span>
          </>
        }
        title="Other hosts"
        align="start"
      >
        {peers.map((p) => (
          <PeerRow key={p.key} peer={p} dense />
        ))}
      </Popover>
    </div>
  );
}
