import type { Meta, StoryObj } from '@storybook/react-vite';
import { waitFor } from 'storybook/test';
import { ActionError } from '../dashboard/api';
import { NOW, quiet } from '../dashboard/fixtures';
import type { DaemonState } from '../popover/types';
import { Maintenance, type MaintenanceProps } from './Maintenance';

// Maintenance (S13, S15) in its states. The shell draws the sidebar; every story feeds the view a
// fixture and a stub `run`.

const ok = async () => {};
const ago = (s: number) => NOW - s * 1000;

const meta = {
  title: 'Screens/Maintenance',
  component: Maintenance,
  args: { state: quiet, now: NOW, run: ok },
  parameters: { layout: 'fullscreen' },
  decorators: [
    (Story) => (
      <div className="min-h-screen w-full bg-bg p-3xl text-fg">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<MaintenanceProps>;
export default meta;
type Story = StoryObj<typeof meta>;

const withBudget = (patch: Partial<NonNullable<DaemonState['budget']>>): DaemonState => ({ ...quiet, budget: { ...quiet.budget, ...patch } });
const withVm = (golden: NonNullable<NonNullable<DaemonState['budget']>['vm']>['golden']): DaemonState =>
  withBudget({ vm: { ...quiet.budget?.vm, golden_required: true, golden } });

const clickButton = (root: ParentNode, label: string) => {
  const b = Array.from(root.querySelectorAll<HTMLElement>('button')).find((x) => x.textContent?.trim() === label);
  if (!b) throw new Error(`no ${label} button`);
  b.click();
};

/** A healthy host claiming jobs: nothing held, a golden image verified, no update on offer. */
export const Running: Story = {};
/** Paused: the button offers Resume. */
export const Paused: Story = { args: { state: { ...quiet, paused: true } } };
/** Draining: the job count, no Pause or Resume, and no Stand down panel and no tile action. */
export const Draining: Story = { args: { state: { ...quiet, draining: true } } };
/** Docker admission held: the reason and when, and Release. */
export const Held: Story = { args: { state: withBudget({ hold: { reason: 'Resizing the Docker VM', since_ms: ago(7200) } }) } };
/** Two repositories held after a trust bypass: each with its own Clear. */
export const WithRepoHolds: Story = {
  args: {
    state: {
      ...quiet,
      repo_holds: [
        { repo: 'owner/demo', reason: 'a trust bypass ran job 77', since_ms: ago(5400), by: 'gate' },
        { repo: 'acme/web', reason: 'a trust bypass ran job 81', since_ms: ago(900), by: 'gate' },
      ],
    },
  },
};
/** No golden image configured: Repository holds takes the full width. */
export const NoGolden: Story = { args: { state: withVm([]) } };
/** A golden image that failed its boot check, and one never verified. */
export const GoldenFailed: Story = {
  args: {
    state: withVm([
      { image: 'macos-golden', standing: 'failed:boot', detail: 'The guest did not boot; run faber golden verify macos-golden after fixing it.' },
      { image: 'macos-next', standing: 'unverified', detail: null },
    ]),
  },
};
/** A release staged for the idle window: Install now (the tile's one primary) and Skip. */
export const UpdateReady: Story = {
  args: {
    state: {
      ...quiet,
      update: { ...quiet.update, available: '0.4.2', candidate_reason: 'newest release', phase: { phase: 'waiting_idle', version: '0.4.2' } },
    },
  },
};
/** Hold… opens the dialog: a reason field and Hold. */
export const HoldDialogOpen: Story = {
  play: async ({ canvasElement }) => {
    clickButton(canvasElement, 'Hold…');
    await waitFor(() => {
      if (!document.querySelector('[role="dialog"]')) throw new Error('the dialog did not open');
    });
  },
};
/** Drain… opens the confirmation, which names the running job count. */
export const DrainConfirmOpen: Story = {
  play: async ({ canvasElement }) => {
    clickButton(canvasElement, 'Drain…');
    await waitFor(() => {
      if (!document.querySelector('[role="dialog"]')) throw new Error('the dialog did not open');
    });
  },
};
/** The daemon refuses Clear (403): its text under the row, and the sudo command to copy. */
export const ActionFailed: Story = {
  args: {
    state: { ...quiet, repo_holds: [{ repo: 'owner/demo', reason: 'a trust bypass ran job 77', since_ms: ago(5400), by: 'gate' }] },
    run: async () => {
      throw new ActionError(403, 'Clearing a hold widens trust: run it with sudo.');
    },
  },
  play: async ({ canvasElement }) => {
    clickButton(canvasElement, 'Clear');
    await waitFor(() => {
      if (!canvasElement.textContent?.includes('sudo faber repo-hold clear owner/demo')) throw new Error('no command shown');
    });
  },
};
