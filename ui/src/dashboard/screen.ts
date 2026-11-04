// The contract between the dashboard shell and its views (docs/design/dashboard/screens.md): the
// shell polls the daemon and runs actions; a view is a pure function of what it is handed, so its
// stories feed it fixtures and a stub `run`.
import type { DaemonState } from '../popover/types';
import * as api from './api';

/** Everything a view can ask the daemon to do. Alarm actions name theirs by `cmd` (dashboard/alarms.ts). */
export type DashboardAction =
  | { cmd: 'reconcile' }
  | { cmd: 'pause' }
  | { cmd: 'resume' }
  | { cmd: 'drain' }
  | { cmd: 'hold'; reason: string }
  | { cmd: 'unhold' }
  | { cmd: 'clear_repo_hold'; repo: string }
  | { cmd: 'apply_update' }
  | { cmd: 'skip_update' }
  | { cmd: 'check_update' }
  | { cmd: 'pin'; req: api.PinRequest }
  | { cmd: 'golden_verify'; image: string }
  | { cmd: 'dismiss_attention'; key: string; ident: string }
  | { cmd: 'restore_attention'; key?: string }
  | { cmd: 'ack_trust'; until_ms: number | null };

/** What the shell hands every view. */
export interface ViewProps {
  state: DaemonState;
  /** Wall-clock ms the view formats ages against. */
  now: number;
  /** Runs an action, then the shell polls at once. Rejects with the daemon's own text (`api.ActionError`). */
  run: (action: DashboardAction) => Promise<void>;
}

/** An alarm's `{cmd, args}` as a {@link DashboardAction}, or null for a command the dashboard does not run. */
export function alarmAction(cmd: string, args?: Record<string, unknown>): DashboardAction | null {
  switch (cmd) {
    case 'unhold':
    case 'apply_update':
    case 'skip_update':
    case 'check_update':
      return { cmd };
    case 'clear_repo_hold':
      return typeof args?.repo === 'string' ? { cmd, repo: args.repo } : null;
    case 'set_pin': {
      // The row's args are the Tauri camelCase shape; the daemon's PinRequest is snake_case.
      if (typeof args?.class !== 'string' || typeof args.runnerMib !== 'number' || typeof args.dindMib !== 'number') return null;
      return { cmd: 'pin', req: { class: args.class, pin: true, runner_mib: args.runnerMib, dind_mib: args.dindMib } };
    }
    case 'dismiss_attention':
      return typeof args?.key === 'string' && typeof args.ident === 'string' ? { cmd, key: args.key, ident: args.ident } : null;
    case 'restore_attention':
      return typeof args?.key === 'string' ? { cmd, key: args.key } : { cmd };
    case 'ack_trust':
      // The Tauri argument is camelCase; the daemon's body is snake_case.
      return typeof args?.untilMs === 'number' || args?.untilMs === null ? { cmd, until_ms: args.untilMs as number | null } : null;
    default:
      return null;
  }
}

/** Sends one action to the daemon. The shell's `run`; views never call it directly. */
export async function send(action: DashboardAction): Promise<void> {
  switch (action.cmd) {
    case 'reconcile':
      await api.reconcile();
      return;
    case 'pause':
      await api.pause();
      return;
    case 'resume':
      await api.resume();
      return;
    case 'drain':
      await api.drain();
      return;
    case 'hold':
      await api.hold(action.reason);
      return;
    case 'unhold':
      await api.unhold();
      return;
    case 'clear_repo_hold':
      await api.clearRepoHold(action.repo);
      return;
    case 'apply_update':
      await api.updateApply();
      return;
    case 'skip_update':
      await api.updateSkip();
      return;
    case 'check_update':
      await api.updateCheck();
      return;
    case 'pin':
      await api.setPin(action.req);
      return;
    case 'golden_verify':
      await api.goldenVerify(action.image);
      return;
    case 'dismiss_attention':
      await api.dismissAttention(action.key, action.ident);
      return;
    case 'restore_attention':
      await api.restoreAttention(action.key);
      return;
    case 'ack_trust':
      await api.ackTrust(action.until_ms);
      return;
  }
}
