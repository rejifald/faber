// Polls /api/state the way the inline dashboard did (every 2 s), never two requests at once, and
// adds the offline indicator it lacked: `stale` once the last success is old or the latest fetch failed.
import { useCallback, useEffect, useRef, useState } from 'react';
import type { DaemonState } from '../popover/types';
import { isSessionEnded } from '../session';
import { getState } from './api';

export const POLL_MS = 2000;
/** A snapshot older than this, or a failed latest fetch, makes the view stale. */
export const STALE_MS = 6000;

/** Whether what is on screen can no longer be trusted as live. Pure, for the tests. */
export function isStale(lastOkMs: number | null, failed: boolean, now: number): boolean {
  return failed || lastOkMs === null || now - lastOkMs > STALE_MS;
}

export interface DaemonStateHandle {
  /** The last good snapshot; kept across failures so the screen dims instead of emptying. */
  state: DaemonState | null;
  lastOkMs: number | null;
  stale: boolean;
  /** Why the session ended (a 401), or null. Polling has stopped; reloading cannot bring a key back. */
  sessionEnded: string | null;
  /** Fetch now, for after an action. Coalesces with a fetch already in flight. */
  refresh: () => Promise<void>;
}

/** `fetchState` is `getState`; stories and tests pass a fake. */
export function useDaemonState(fetchState: () => Promise<DaemonState> = getState): DaemonStateHandle {
  const [state, setState] = useState<DaemonState | null>(null);
  const [lastOkMs, setLastOkMs] = useState<number | null>(null);
  const [failed, setFailed] = useState(false);
  const [sessionEnded, setSessionEnded] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  const fetchRef = useRef(fetchState);
  const inflight = useRef<Promise<void> | null>(null);
  const again = useRef(false);
  const ended = useRef(false);
  const alive = useRef(true);

  useEffect(() => {
    fetchRef.current = fetchState;
  }, [fetchState]);

  const run = useCallback((): Promise<void> => {
    if (ended.current) return Promise.resolve();
    // A request is out: ask for one more behind it, so an action's refresh sees the action.
    if (inflight.current) {
      again.current = true;
      return inflight.current;
    }
    const p = (async () => {
      // One request at a time; a refresh asked for meanwhile runs right behind it.
      do {
        again.current = false;
        try {
          const next = await fetchRef.current();
          if (!alive.current) break;
          setState(next);
          setLastOkMs(Date.now());
          setFailed(false);
        } catch (e) {
          if (!alive.current) break;
          if (isSessionEnded(e)) {
            ended.current = true;
            setSessionEnded(e.why);
          }
          setFailed(true);
        }
      } while (again.current && alive.current && !ended.current);
      inflight.current = null;
    })();
    inflight.current = p;
    return p;
  }, []);

  useEffect(() => {
    alive.current = true;
    void run();
    const poll = setInterval(() => void run(), POLL_MS);
    // Staleness depends on the clock, not only on events: tick so it flips with no new data.
    const tick = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      alive.current = false;
      clearInterval(poll);
      clearInterval(tick);
    };
  }, [run]);

  return { state, lastOkMs, stale: isStale(lastOkMs, failed, now), sessionEnded, refresh: run };
}
