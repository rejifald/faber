// The daemon calls the dashboard makes (src/web.rs routes). Every call carries the session key; a
// 401 throws SessionEnded. A refusal or failure throws an ActionError carrying the daemon's text.
import type { DaemonState } from '../popover/types';
import { call, headers } from '../session';

export { isSessionEnded, SessionEnded } from '../session';

/** A non-2xx answer: `message` is the daemon's own text (a 403 says why a clear needs sudo). */
export class ActionError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ActionError';
    this.status = status;
  }
}

async function fail(res: Response): Promise<never> {
  const text = (await res.text().catch(() => '')).trim();
  throw new ActionError(res.status, text || `faber answered ${res.status}`);
}

async function post(path: string, body?: unknown): Promise<Response> {
  const res = await call(path, {
    method: 'POST',
    headers: headers(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) return fail(res);
  return res;
}

/** The daemon's one-word answer (`paused`, `triggered`, `draining 2`). */
const word = async (path: string, body?: unknown): Promise<string> => (await (await post(path, body)).text()).trim();

/** The state snapshot the action returned: fresher than the next poll. */
const snapshot = async (path: string, body?: unknown): Promise<DaemonState> => (await (await post(path, body)).json()) as DaemonState;

/** GET /api/state. */
export async function getState(): Promise<DaemonState> {
  const res = await call('/api/state', { headers: headers() });
  if (!res.ok) return fail(res);
  return (await res.json()) as DaemonState;
}

/** Run a reconcile cycle now. */
export const reconcile = () => word('/api/reconcile');

/** Stop starting anything new, Docker or macOS VM, until resumed; running jobs finish. */
export const pause = () => word('/api/pause');

/** Lift a pause, whoever set it; parked claims go at once. */
export const resume = () => word('/api/resume');

/** Stand down for good (ADR-0011 § 5); answers `draining <n left>`. Not a pause. */
export const drain = () => word('/api/drain');

/** Hold Docker admission. The reason is trimmed, never blank, and at most 200 characters; the daemon refuses otherwise (400). */
export const hold = (reason: string) => snapshot('/api/hold', { reason });

/** Release the Docker hold, if there is one. */
export const unhold = () => snapshot('/api/unhold');

/** Clear a repo's minting hold (ADR-0030). Widens trust, so the daemon may refuse with a 403 and the reason. */
export const clearRepoHold = (repo: string) => snapshot('/api/repo-hold/clear', { repo });

/**
 * Pin a class's caps to these values, or return it to the learner (`pin: false`). A `0` CPU leaves
 * that container's CPU learned. Answers the state without the updater's view.
 */
export interface PinRequest {
  class: string;
  pin: boolean;
  runner_mib?: number;
  dind_mib?: number;
  runner_cpus?: number;
  dind_cpus?: number;
}
export const setPin = (req: PinRequest) => snapshot('/api/pin', req);

/** Drain and install the staged release now, instead of waiting for an idle window. */
export const updateApply = () => snapshot('/api/update/apply');

/** Refuse the available version on this host; the updater moves on. */
export const updateSkip = () => snapshot('/api/update/skip');

/** Poll for a release now. */
export const updateCheck = () => snapshot('/api/update/check');

/** Verify a golden image now (ADR-0049 § 7). Answers at once: `verifying <image>` or `already verifying <image>`; 404 names an image no profile uses, 409 says VM jobs are not configured. */
export const goldenVerify = (image: string) => word('/api/golden/verify', { image });

/** Hide an attention row while its key keeps this identity (ADR-0064 § 2); replaces the key's earlier dismissal. */
export const dismissAttention = (key: string, ident: string) => snapshot('/api/attention/dismiss', { key, ident });

/** Show a dismissed row again, or every dismissed row when `key` is absent. */
export const restoreAttention = (key?: string) => snapshot('/api/attention/restore', key === undefined ? {} : { key });

/** Acknowledge every trust change at or before `untilMs` (ADR-0064 § 4); `null` withdraws the acknowledgement. */
export const ackTrust = (untilMs: number | null) => snapshot('/api/attention/ack-trust', { until_ms: untilMs });
