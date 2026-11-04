// Other hosts (popover/anatomy.md § 2, Peer host). The VM section is shaped in vms.ts.
import { fmtAgo, plural, spokenAgo } from './format';
import type { Pip } from './groups';
import type { Peer } from './types';

/** A peer not seen in a runner listing for this long is drawn amber: its runners may be gone. */
export const PEER_STALE_MS = 5 * 60 * 1000;
/** Peer rows shown before the rest fold into "+N". */
export const PEERS_MAX = 2;

export interface PeerRowModel {
  key: string;
  name: string;
  runners: number;
  /** A running pip per busy runner, then an idle one per idle runner; offline runners get none.
   * Empty while stale: after five minutes nobody knows which runners still exist. From an older
   * daemon with no busy count, every runner is idle: a count that claims no work. */
  pips: Pip[];
  stale: boolean;
  age: string;
  label: string;
}

export function peerRows(peers: Peer[] | undefined, now: number): PeerRowModel[] {
  return (peers || []).map((p) => {
    // `runners` is repo → count; the row shows the host's total.
    const sum = (m: Record<string, number> | undefined) => Object.values(m || {}).reduce((a, n) => a + (n || 0), 0);
    const runners = sum(p.runners);
    const known = p.busy !== undefined || p.idle !== undefined;
    const busy = known ? sum(p.busy) : 0;
    const idle = known ? sum(p.idle) : runners;
    const ago = Math.max(0, (now - (p.last_seen_ms || 0)) / 1000);
    const stale = now - (p.last_seen_ms || 0) > PEER_STALE_MS;
    return {
      key: p.instance,
      name: `peer ${p.instance}`,
      runners,
      pips: stale
        ? []
        : [
            ...Array.from({ length: busy }, (_, i): Pip => ({ kind: 'running', key: `${p.instance}-b${i}` })),
            ...Array.from({ length: idle }, (_, i): Pip => ({ kind: 'idle', key: `${p.instance}-i${i}` })),
          ],
      stale,
      age: fmtAgo(ago),
      // Stale, the row shows `not seen` and no pips: the label says so, and gives the last count
      // as last seen, not as now.
      label: (stale
        ? [`peer ${p.instance}`, `not seen for ${spokenAgo(ago)}`, `last seen with ${plural(runners, 'runner')}`]
        : [`peer ${p.instance}`, plural(runners, 'runner'), known ? `${busy} busy` : null, `seen ${spokenAgo(ago)} ago`]
      )
        .filter(Boolean)
        .join(', '),
    };
  });
}
