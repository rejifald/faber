import type { Meta, StoryObj } from '@storybook/react-vite';
import { jobsEmpty, jobsWeek } from '../dashboard/fixtures';
import { Jobs } from './Jobs';

// Jobs (/jobs, ADR-0056 § 5): the main column only, the shell draws the sidebar. Each story hands
// the screen a stub loader in place of `/api/jobs`.

const meta = {
  title: 'Screens/Jobs',
  component: Jobs,
  parameters: { layout: 'fullscreen' },
  args: { load: async () => jobsWeek },
  decorators: [
    (Story) => (
      <div className="min-h-screen w-full bg-bg p-2xl">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof Jobs>;
export default meta;
type Story = StoryObj<typeof meta>;

/** A week of four classes: parked, throttled, macOS and one still profiling. */
export const Busy: Story = {};

/** The audit log is still being read. */
export const Loading: Story = { args: { load: () => new Promise(() => undefined) } };

/** No run finished in the window. */
export const Empty: Story = { args: { load: async () => jobsEmpty } };

/** The daemon could not answer. */
export const Failed: Story = { args: { load: async () => Promise.reject(new Error('faber answered 500')) } };

/** A class with fewer than 20 runs: badged and sorted last. */
export const ProfilingClass: Story = { args: { load: async () => ({ ...jobsWeek, classes: jobsWeek.classes.slice(3) }) } };

/** A macOS class: phases and steps, its resources not measured. */
export const MacosClass: Story = { args: { load: async () => ({ ...jobsWeek, classes: jobsWeek.classes.slice(2, 3) }) } };

/** The last state poll is old: the header says Stale. */
export const Stale: Story = { args: { stale: true } };
