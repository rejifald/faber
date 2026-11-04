import type { Meta, StoryObj } from '@storybook/react-vite';
import { fmtMs, runSegments } from '../dashboard/jobs';
import { runFootprint, runFootprintPartial } from '../dashboard/fixtures';
import { PhaseBar } from './PhaseBar';

// One story per state in PhaseBar's @states, from the Activity run fixtures (ADR-0056).

const meta = {
  title: 'Kit/PhaseBar',
  component: PhaseBar,
  args: { segments: runSegments(runFootprint), format: fmtMs },
  decorators: [
    (Story) => (
      <div className="w-full max-w-popover-wide rounded-lg border border-border bg-surface p-lg">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof PhaseBar>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Every phase measured, the job split by GitHub's steps, with the legend. */
export const Full: Story = {};

/** The thin bar a table cell holds: no legend, the label is the text. */
export const Compact: Story = { args: { compact: true } };

/** Teardown and the step split were not measured: listed, not drawn. */
export const UnmeasuredPhases: Story = {
  args: { segments: runSegments({ ...runFootprintPartial, phases: runFootprintPartial.phases.map((p) => ({ ...p, skewed: false })) }) },
};

/** Pickup crossed clocks and came out negative: zero, and the legend says the clocks disagree. Everything else measured. */
export const SkewedPhase: Story = {
  args: { segments: runSegments({ ...runFootprint, phases: runFootprint.phases.map((p) => (p.phase === 'pickup' ? { ...p, ms: 0, skewed: true } : p)) }) },
};

/** Nothing measured at all: an empty track and "not measured" throughout. */
export const NothingMeasured: Story = {
  args: { segments: runSegments({ ...runFootprint, split: null, phases: runFootprint.phases.map((p) => ({ ...p, ms: null })) }) },
};
