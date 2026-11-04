import type { Meta, StoryObj } from '@storybook/react-vite';
import { SampleBars } from './SampleBars';

// One story per state in SampleBars' @states.

const meta = { title: 'Kit/SampleBars', component: SampleBars, args: { count: 89 } } satisfies Meta<typeof SampleBars>;
export default meta;
type Story = StoryObj<typeof meta>;

/** No samples: four empty ticks. */
export const Empty: Story = { args: { count: 0 } };

/** Under the threshold: some ticks filled, amber. */
export const Learning: Story = { args: { count: 3 } };

/** At or over the threshold: all four filled, neutral. */
export const Learned: Story = {};
