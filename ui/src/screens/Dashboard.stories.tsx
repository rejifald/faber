import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { full, jobsWeek, NOW, quiet, runFootprint } from '../dashboard/fixtures';
import { Dashboard, type DashboardProps, type ViewId } from './Dashboard';

// One story per state in Dashboard's @states. The sidebar routes between views in place.

function Live(props: Partial<DashboardProps> & { start?: ViewId }) {
  const [view, setView] = useState<ViewId>(props.start ?? 'overview');
  return (
    <Dashboard
      state={full}
      now={NOW}
      stale={false}
      sessionEnded={null}
      run={() => Promise.resolve()}
      loadJobs={async () => jobsWeek}
      loadRun={async () => runFootprint}
      {...props}
      view={view}
      onNavigate={setView}
    />
  );
}

const meta = {
  title: 'Screens/Dashboard',
  component: Dashboard,
  parameters: { layout: 'fullscreen' },
  args: { state: full, now: NOW, stale: false, sessionEnded: null, run: () => Promise.resolve(), view: 'overview', onNavigate: () => {} },
} satisfies Meta<typeof Dashboard>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Connecting: Story = { render: () => <Live state={null} /> };
export const Overview: Story = { render: () => <Live /> };
export const Activity: Story = { render: () => <Live start="activity" /> };
export const Jobs: Story = { render: () => <Live start="jobs" /> };
export const Memory: Story = { render: () => <Live start="memory" /> };
export const Maintenance: Story = { render: () => <Live start="maintenance" /> };
export const Stale: Story = { render: () => <Live stale /> };
export const SessionEnded: Story = { render: () => <Live sessionEnded="The daemon restarted (an update or a restart)" /> };
/** The full fixture has an update ready; the quiet host has none, so its sidebar foot is empty. */
export const UpdatePending: Story = { render: () => <Live /> };
export const Quiet: Story = { render: () => <Live state={quiet} /> };
