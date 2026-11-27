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

// DRAFT (speed-up hints, for the owner's review): illustrative figures, not a real host's.
const checkout = { kind: 'git_mirror', window_days: 7, runs: 300, median_ms: 24_000, total_ms: 7_500_000 };

/** A hint on a healthy host: the git mirror is held off beside dind, with what checkout costs here. */
export const SpeedHintHeld: Story = { args: { state: { ...quiet, hints: [{ ...checkout, cause: 'engine_marked' }] } } };
/** The same capability simply off: no profile asks for it. The fix widens trust, and the row says so. */
export const SpeedHintOff: Story = { args: { state: { ...quiet, hints: [{ ...checkout, cause: 'off' }] } } };
/** A profile grants dind now. */
export const SpeedHintDindGranted: Story = { args: { state: { ...quiet, hints: [{ ...checkout, cause: 'dind_granted', profiles: ['linux'] }] } } };
/** No timed run yet: the row says so and states no figure. */
export const SpeedHintNoRecord: Story = { args: { state: { ...quiet, hints: [{ kind: 'git_mirror', cause: 'off', window_days: 7 }] } } };
/** Among real alarms the hint sorts last: neutral, after red and amber. */
export const SpeedHintAmongAlarms: Story = { args: { state: { ...full, hints: [{ ...checkout, cause: 'engine_marked' }] } } };
