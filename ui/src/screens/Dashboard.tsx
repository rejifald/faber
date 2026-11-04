import { Activity as ActivityIcon, ChartColumn, ChartNoAxesGantt, LayoutDashboard, Settings as SettingsIcon, Wrench } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { DashboardNav, type DashboardNavItem } from '../blocks/DashboardNav';
import { HostCard } from '../blocks/HostCard';
import { UpdateCard } from '../blocks/UpdateCard';
import { activeAlarms } from '../dashboard/alarms';
import { navState, updateModel } from '../dashboard/maintenance';
import type { DashboardAction } from '../dashboard/screen';
import type { StatusTone } from '../kit';
import { peerRows } from '../popover/peers';
import type { DaemonState } from '../popover/types';
import { Activity, type ActivityProps } from './Activity';
import { Jobs, type JobsProps } from './Jobs';
import { Maintenance } from './Maintenance';
import { Memory } from './Memory';
import { Overview } from './Overview';

/** The views the shell routes between (ADR-0054 § 1, ADR-0056 § 5). Settings is its own page at `/settings`. */
export type ViewId = 'overview' | 'activity' | 'jobs' | 'memory' | 'maintenance';

/** The flat path of each view (ADR-0054 § 5): one segment, never a trailing slash. */
export const viewPaths: Record<ViewId, string> = { overview: '/', activity: '/activity', jobs: '/jobs', memory: '/memory', maintenance: '/maintenance' };

/** The view a path names; anything else is Overview, as `/` is. */
export function viewOf(path: string): ViewId {
  const found = (Object.keys(viewPaths) as ViewId[]).find((id) => viewPaths[id] === path);
  return found ?? 'overview';
}

export interface DashboardProps {
  /** The last good snapshot, or null before the first poll answers. */
  state: DaemonState | null;
  /** Wall-clock ms the views format ages against. */
  now: number;
  /** The last good poll is too old (screens.md S10). */
  stale: boolean;
  /** Why the session ended (a 401), or null. */
  sessionEnded: string | null;
  /** Runs an action, then polls. */
  run: (action: DashboardAction) => Promise<void>;
  /** The view shown; the shell changes it as the sidebar is used. */
  view: ViewId;
  /** Called with the view the sidebar chose. The entry pushes its path. */
  onNavigate: (view: ViewId) => void;
  /** Reads the Jobs view. The daemon's `/api/jobs` when omitted; stories pass fixtures. */
  loadJobs?: JobsProps['load'];
  /** Reads one run's phases for Activity. The daemon's `/api/runs/:runner` when omitted; stories pass fixtures. */
  loadRun?: ActivityProps['loadRun'];
}

/** Each view's screen, by id. */
type ShellProps = Pick<DashboardProps, 'now' | 'run' | 'stale' | 'loadJobs' | 'loadRun'> & { state: DaemonState };
const VIEWS: Record<ViewId, (props: ShellProps) => ReactNode> = {
  overview: (p) => <Overview {...p} />,
  activity: ({ loadRun, ...p }) => <Activity {...p} {...(loadRun ? { loadRun } : {})} />,
  jobs: ({ loadJobs, stale }) => <Jobs stale={stale} {...(loadJobs ? { load: loadJobs } : {})} />,
  memory: (p) => <Memory {...p} />,
  maintenance: (p) => <Maintenance {...p} />,
};

/** The host's one-dot reading: red on a bad alarm or an unhealthy daemon, amber on a warning, else green. A dismissed alarm does not count. */
function hostTone(state: DaemonState, now: number): StatusTone {
  if (state.healthy === false) return 'red';
  const alarms = activeAlarms(state, now).active;
  if (alarms.some((a) => a.sev === 'bad')) return 'red';
  if (alarms.some((a) => a.sev === 'warn')) return 'amber';
  return 'green';
}

function hostSummary(state: DaemonState, now: number): string {
  const peers = peerRows(state.peers, now);
  if (peers.length === 0) return '1 host';
  const stale = peers.filter((p) => p.stale).length;
  return stale ? `${peers.length + 1} hosts, ${stale} not seen` : `${peers.length + 1} hosts`;
}

function navItems(state: DaemonState): DashboardNavItem[] {
  const faults = (state.faults ?? []).length;
  const budget = state.budget;
  const flag = navState(state);
  const pct = budget?.budget_mib ? Math.round(((budget.reserved_mib ?? 0) / budget.budget_mib) * 100) : null;
  return [
    { id: 'overview', label: 'Overview', icon: LayoutDashboard, href: viewPaths.overview },
    {
      id: 'activity',
      label: 'Activity',
      icon: ActivityIcon,
      href: viewPaths.activity,
      ...(faults ? { count: faults, countTone: 'amber' as const, countLabel: `${faults} ${faults === 1 ? 'fault' : 'faults'}` } : {}),
    },
    { id: 'jobs', label: 'Jobs', icon: ChartNoAxesGantt, href: viewPaths.jobs },
    {
      id: 'memory',
      label: 'Memory',
      icon: ChartColumn,
      href: viewPaths.memory,
      ...(pct !== null ? { count: `${pct}%`, countLabel: `${pct} percent of the budget reserved` } : {}),
    },
    {
      id: 'maintenance',
      label: 'Maintenance',
      icon: Wrench,
      href: viewPaths.maintenance,
      ...(flag ? { count: flag.count, countKind: 'word' as const, countTone: 'amber' as const, countLabel: flag.label } : {}),
    },
    // Repositories joins the sidebar with its own step (ADR-0054 § 2).
    { id: 'settings', label: 'Settings', icon: SettingsIcon, href: '/settings' },
  ];
}

/**
 * The dashboard: the sidebar (brand, host card, the views, an update card) and the view chosen in
 * it. A pure function of the polled state; the entry polls, routes and runs actions.
 *
 * @useWhen the dashboard entry at `/`, `/activity`, `/jobs`, `/memory` and `/maintenance`. Settings is its own page.
 * @states connecting, overview, activity, jobs, memory, maintenance, stale, session ended, update pending, quiet
 */
export function Dashboard({ state, now, stale, sessionEnded, run, view, onNavigate, loadJobs, loadRun }: DashboardProps) {
  const [updatePending, setUpdatePending] = useState<'install' | 'skip'>();
  const [updateError, setUpdateError] = useState<string>();

  if (sessionEnded) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-bg p-xl font-sans text-body text-fg">
        <p className="max-w-popover-wide text-center">
          {sessionEnded}. Run <code className="font-mono">faber dashboard</code> or open the dashboard from the
          menu-bar app.
        </p>
      </main>
    );
  }
  if (!state) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-bg p-xl font-sans text-body text-muted">
        <p role="status">Connecting to faber…</p>
      </main>
    );
  }

  const update = state.update;
  // The card shows only what the Updates panel offers as Install now, in the panel's own words.
  const offer = updateModel(state, now);
  const ready = offer?.canInstall && offer.available ? { version: offer.available.version, body: offer.phase?.word ?? '', canSkip: offer.canSkip } : null;
  const updateAct = async (kind: 'install' | 'skip') => {
    setUpdatePending(kind);
    setUpdateError(undefined);
    try {
      await run({ cmd: kind === 'install' ? 'apply_update' : 'skip_update' });
    } catch (e) {
      setUpdateError(e instanceof Error ? e.message : String(e));
    } finally {
      setUpdatePending(undefined);
    }
  };

  const props = { state, now, run, stale, loadJobs, loadRun };
  return (
    <div className="flex min-h-screen w-full flex-col bg-bg font-sans text-body text-fg narrow:flex-row">
      <div className="flex shrink-0 narrow:sticky narrow:top-0 narrow:h-screen">
        <DashboardNav
          mode="page"
          items={navItems(state)}
          selected={view}
          onSelect={(id) => {
            if (id === 'settings') location.assign('/settings');
            else onNavigate(id as ViewId);
          }}
          version={update?.current ?? ''}
          host={<HostCard name={state.instance ?? 'this host'} tone={hostTone(state, now)} summary={hostSummary(state, now)} peers={peerRows(state.peers, now)} />}
          footer={
            ready ? (
              <UpdateCard
                version={ready.version}
                body={ready.body}
                href={viewPaths.maintenance}
                onDetails={() => onNavigate('maintenance')}
                onInstall={() => void updateAct('install')}
                onSkip={ready.canSkip ? () => void updateAct('skip') : undefined}
                pending={updatePending}
                error={updateError}
              />
            ) : undefined
          }
        />
      </div>
      <main className="min-w-0 flex-1 p-xl narrow:p-3xl">
        {VIEWS[view](props)}
      </main>
    </div>
  );
}
