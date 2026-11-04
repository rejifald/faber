import type { Meta, StoryObj } from '@storybook/react-vite';
import { runFootprint, runFootprintPartial } from '../dashboard/fixtures';
import type { RunFootprint } from '../dashboard/jobs';
import { RunPhases } from './RunPhases';

// RunPhases' @states, each fed by a stub loader in place of `/api/runs/:runner`.

const meta = {
  title: 'Blocks/RunPhases',
  component: RunPhases,
  args: { runner: runFootprint.runner, load: async (): Promise<RunFootprint | null> => runFootprint },
  decorators: [
    (Story) => (
      <div className="w-full max-w-popover-wide rounded-lg border border-border bg-surface p-lg">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof RunPhases>;
export default meta;
type Story = StoryObj<typeof meta>;

/** The read is still in flight. */
export const Loading: Story = { args: { load: () => new Promise(() => undefined) } };

/** A Docker run, every phase and resource measured. */
export const Measured: Story = {};

/** A macOS run: one phase skewed, teardown and steps not read, no resources. */
export const PartlyMeasured: Story = { args: { load: async () => runFootprintPartial } };

/** A run older than the footprint fields. */
export const NotRecorded: Story = { args: { load: async () => null } };

/** The daemon refused the read. */
export const Failed: Story = { args: { load: async () => Promise.reject(new Error('faber answered 500')) } };
