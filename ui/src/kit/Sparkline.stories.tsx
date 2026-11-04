import type { Meta, StoryObj } from '@storybook/react-vite';
import { Sparkline, type SparklineTone } from './Sparkline';

// One story per state in Sparkline's @states, plus each tone.

const meta = {
  title: 'Kit/Sparkline',
  component: Sparkline,
  args: { label: 'Delivered rate over 24 hours, rising', values: [2, 3, 3, 5, 4, 6, 7, 9] },
} satisfies Meta<typeof Sparkline>;
export default meta;
type Story = StoryObj<typeof meta>;

const TONES: SparklineTone[] = ['neutral', 'green', 'amber', 'red', 'accent'];

/** A series: the line fills the box from its minimum to its maximum. */
export const Data: Story = {};

/** One value cannot make a line: a dot, centred. */
export const OnePoint: Story = { args: { values: [5], label: 'Queue depth, one sample' } };

/** No values: nothing is drawn, the box keeps its size so the tile does not shift. */
export const Empty: Story = { args: { values: [], label: 'Queue depth, no data yet' } };

/** Every value equal: a straight line across the middle. */
export const Flat: Story = { args: { values: [4, 4, 4, 4, 4, 4], label: 'Parked jobs over 24 hours, steady' } };

/** The five tones on one series. */
export const Tones: Story = {
  render: (args) => (
    <div className="flex items-center gap-lg">
      {TONES.map((tone) => (
        <Sparkline key={tone} {...args} tone={tone} label={`${tone} trend`} />
      ))}
    </div>
  ),
};
