import type { Meta, StoryObj } from '@storybook/react-vite';
import { RunStrip } from './RunStrip';

// One story per state in RunStrip's @states. Peaks are in MiB.

const runs = [3000, 3100, 3200, 3000, 8200, 3100, 3300, 3000, 8300, 3100, 3200, 3000, 3100, 3200];

const meta = {
  title: 'Kit/RunStrip',
  component: RunStrip,
  args: { peaks: runs.map((p) => (p > 8000 ? p : p - 400)), capMib: 3500 },
  decorators: [
    (Story) => (
      <div className="w-rail">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof RunStrip>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Runs under the cap are amber. */
export const Data: Story = { args: { peaks: [2600, 2800, 3000, 2700, 3100, 2900], capMib: 3500 } };

/** No runs: nothing is drawn, and the summary says so. */
export const NoRuns: Story = { args: { peaks: [] } };

/** Runs past the cap are red. */
export const OverTheCap: Story = {};

/** A run cut short is red whatever its measured peak. */
export const Censored: Story = {
  args: { peaks: [2600, 2800, 1200, 2700], capMib: 3500, censored: [false, false, true, false] },
};

/** The axis reads "oldest" at the left and the caller's label at the right. */
export const WithEndLabel: Story = { args: { peaks: [2600, 2800, 3000, 2700, 3100, 2900], capMib: 3500, endLabel: '6m ago' } };
