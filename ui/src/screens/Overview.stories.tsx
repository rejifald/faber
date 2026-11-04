import type { Meta, StoryObj } from '@storybook/react-vite';
import { waitFor } from 'storybook/test';
import { dismissedAlarms, full, NOW, quiet } from '../dashboard/fixtures';
import type { DaemonState } from '../popover/types';
import { Overview, type OverviewProps } from './Overview';

// The Overview at the dashboard's primary width (screens.md § 6): the main column only, the shell
// draws the sidebar. Every story feeds it a fixture and a stub `run`.

const ok = async () => {};
const fails = async () => {
  throw new Error('The daemon refused: not the owner.');
};

/** A host that has just started: no runs, no classes, nothing in flight, counts at zero. */
const fresh: DaemonState = {
  ...quiet,
  uptime_s: 42,
  counts: { provisioned: 0, completed: 0, faults: 0, refused: 0, retried: 0, dind_provided: 0, dind_withheld: 0, dind_refused_total: 0, service_refused_total: 0 },
  inflight: [],
  outcomes: [],
  recent: [],
  retries: [],
  git_mirrors: [],
  budget: { ...quiet.budget, classes: [], reserved_mib: 0, docker_inflight: 0, gates: {} },
};

const meta = {
  title: 'Screens/Overview',
  component: Overview,
  args: { state: full, now: NOW, run: ok },
  parameters: { layout: 'fullscreen' },
  decorators: [
    (Story) => (
      <div className="min-h-screen w-full bg-bg p-3xl text-fg">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<OverviewProps>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Every alarm, a parked and a frozen job, faults and refusals in the feed. */
export const Busy: Story = {};
/** One job running, nothing to attend to. */
export const Healthy: Story = { args: { state: quiet } };
/** A fresh host: each panel says what will appear in it. */
export const Empty: Story = { args: { state: fresh } };
/** The last good poll is older than the staleness bound. */
export const Stale: Story = { args: { state: quiet, stale: true } };
/** An alarm's own fix, refused by the daemon: the card reports it under its text. */
export const ActionFailed: Story = {
  args: { run: fails },
  play: async ({ canvasElement }) => {
    const release = Array.from(canvasElement.querySelectorAll<HTMLElement>('button')).find((b) => b.textContent === 'Release');
    if (!release) throw new Error('no Release button on the hold alarm');
    release.click();
    await waitFor(() => {
      if (!canvasElement.textContent?.includes('The daemon refused')) throw new Error('no failure shown');
    });
  },
};

/**
 * ADR-0064: three alarms dismissed fold into "3 dismissed" under the cards; the one trust change,
 * acknowledged, is a quiet neutral card that opens to list it. Blocking alarms carry no Dismiss.
 */
export const AlarmsDismissed: Story = { args: { state: dismissedAlarms } };

/** The dismissed alarms opened, each with Restore, then Restore all. */
export const DismissedOpen: Story = { args: { state: dismissedAlarms, defaultDismissedOpen: true } };
