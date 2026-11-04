import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pause } from 'lucide-react';
import { Button } from '../kit';
import { ControlTile } from './ControlTile';

// ControlTile as Maintenance draws it (dashboard/screens.md S15): one tile per controllable thing.

const meta = {
  title: 'Blocks/ControlTile',
  component: ControlTile,
  decorators: [
    (Story) => (
      <div className="grid grid-cols-4 gap-lg">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof ControlTile>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Calm: Story = {
  args: { label: 'Host', state: 'Running', tone: 'neutral', note: 'Claiming jobs' },
  render: (args) => (
    <ControlTile {...args}>
      <Button icon={Pause}>Pause</Button>
    </ControlTile>
  ),
};
export const Amber: Story = {
  args: { label: 'Docker admission', state: 'Held', tone: 'amber', note: 'Resizing the Docker VM. Since today 10:30' },
  render: (args) => (
    <ControlTile {...args}>
      <Button>Release</Button>
    </ControlTile>
  ),
};
export const Red: Story = {
  args: { label: 'Updates', state: 'Update failed', tone: 'red', note: 'The signature did not verify' },
  render: (args) => (
    <ControlTile {...args}>
      <Button>Check now</Button>
    </ControlTile>
  ),
};
export const Accent: Story = {
  args: { label: 'Updates', state: '0.4.2 available', tone: 'accent', note: 'Installed 0.4.1' },
};
export const Plain: Story = {
  args: { label: 'Jobs', state: '1 running', note: 'Reconcile asks GitHub for queued jobs now.' },
  render: (args) => (
    <ControlTile {...args}>
      <Button>Reconcile</Button>
    </ControlTile>
  ),
};
export const WithoutNote: Story = {
  args: { label: 'Docker admission', state: 'Open', tone: 'neutral' },
  render: (args) => (
    <ControlTile {...args}>
      <Button>Hold…</Button>
    </ControlTile>
  ),
};
export const NoAction: Story = { args: { label: 'Host', state: 'Draining', tone: 'amber', note: '2 jobs still running' } };
export const BusyAction: Story = {
  args: { label: 'Host', state: 'Running', tone: 'neutral', note: 'Claiming jobs' },
  render: (args) => (
    <ControlTile {...args}>
      <Button icon={Pause} loading>
        Pause
      </Button>
    </ControlTile>
  ),
};
export const ActionFailed: Story = {
  args: { label: 'Host', state: 'Running', tone: 'neutral', note: 'Claiming jobs', error: 'Pause failed: the daemon did not answer.' },
  render: (args) => (
    <ControlTile {...args}>
      <Button icon={Pause}>Pause</Button>
    </ControlTile>
  ),
};

/** Four tiles in a row: equal height, actions on the bottom edge. */
export const Row: Story = {
  args: { label: '', state: '' },
  render: () => (
    <>
      <ControlTile label="Host" state="Running" note="Claiming jobs">
        <Button icon={Pause}>Pause</Button>
      </ControlTile>
      <ControlTile label="Docker admission" state="Open" note="Docker jobs are admitted as the budget allows.">
        <Button>Hold…</Button>
      </ControlTile>
      <ControlTile label="Updates" state="0.4.2 available" tone="accent" note={<span className="block">Installed <span className="font-mono">0.4.1</span></span>}>
        <Button variant="primary">Install now</Button>
      </ControlTile>
      <ControlTile label="Jobs" state="2 running" note="Reconcile asks GitHub for queued jobs now, instead of waiting for the next cycle.">
        <Button>Reconcile</Button>
      </ControlTile>
    </>
  ),
};
