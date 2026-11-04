// The session plumbing every page that talks to the daemon shares (settings/api.ts, dashboard/api.ts):
// the key header, the 12 h session and the 401 that ends it.

/** The session key the redeem page stored (src/control.rs); the inline dashboard sent it the same way. */
export function headers(extra: Record<string, string> = {}): Record<string, string> {
  let key = '';
  try {
    key = sessionStorage.getItem('faber-key') ?? '';
  } catch {
    // sessionStorage can be blocked; the daemon then answers 401 and the view says the session ended.
  }
  return { 'X-Faber-Session': key, ...extra };
}

/** Sessions live 12 h, in the daemon's memory only (ADR-0028 addendum C). */
export const SESSION_TTL_MS = 12 * 3600 * 1000;

/**
 * Why a session ended, as the inline dashboard says it: one younger than 12 h can only have ended
 * with the daemon. `since` is when the redeem page stored the key (`faber-since`).
 */
export function sessionEndedWhy(since: string | null, now: number): string {
  const t = Number(since);
  return since && Number.isFinite(t) && now - t < SESSION_TTL_MS ? 'The daemon restarted (an update or a restart)' : 'Session older than 12 h';
}

/** A 401: the session is gone. Thrown, not retried: reloading cannot bring a key back. */
export class SessionEnded extends Error {
  readonly why: string;
  constructor(why: string) {
    super(why);
    this.name = 'SessionEnded';
    this.why = why;
  }
}

/** Whether `e` is a {@link SessionEnded}, by name, so a copy of this module loaded twice still agrees. */
export const isSessionEnded = (e: unknown): e is SessionEnded => e instanceof Error && e.name === 'SessionEnded';

/** `fetch` with the same-origin cookie; a 401 throws {@link SessionEnded}. */
export async function call(url: string, init: RequestInit): Promise<Response> {
  const res = await fetch(url, { ...init, credentials: 'same-origin' });
  if (res.status === 401) {
    let since: string | null = null;
    try {
      since = sessionStorage.getItem('faber-since');
    } catch {
      // Blocked storage: no start time, so the older-than-12-h reason.
    }
    throw new SessionEnded(sessionEndedWhy(since, Date.now()));
  }
  return res;
}
