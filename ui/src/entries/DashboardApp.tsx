import { useCallback, useEffect, useState } from 'react';
import { send, type DashboardAction } from '../dashboard/screen';
import { useDaemonState } from '../dashboard/useDaemonState';
import { Dashboard, viewPaths, viewOf, type ViewId } from '../screens/Dashboard';

/**
 * The dashboard entry's state: it polls the daemon, runs actions and keeps the view in the URL
 * (ADR-0054 § 5), so back and forward move between views and a view can be linked.
 */
export function DashboardApp() {
  const { state, stale, sessionEnded, refresh } = useDaemonState();
  const [view, setView] = useState<ViewId>(() => viewOf(location.pathname));
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const onPop = () => setView(viewOf(location.pathname));
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  // Ages re-render with each poll; this keeps them moving between polls too.
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const navigate = useCallback((next: ViewId) => {
    // A view's query (Activity's filters, Memory's class) belongs to that view, so it is dropped.
    if (location.pathname !== viewPaths[next] || location.search) history.pushState(null, '', viewPaths[next]);
    setView(next);
    window.scrollTo(0, 0);
  }, []);

  const run = useCallback(
    async (action: DashboardAction) => {
      try {
        await send(action);
      } finally {
        await refresh();
      }
    },
    [refresh],
  );

  return <Dashboard state={state} now={now} stale={stale} sessionEnded={sessionEnded} run={run} view={view} onNavigate={navigate} />;
}
