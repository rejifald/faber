import type { Meta, StoryObj } from '@storybook/react-vite';
import { SaveBar } from './SaveBar';

// SaveBar's @states: shown (live, restart, mixed with "and N more") and saving.

const live = { key: 'autolimit.headroom_mib', label: 'Headroom', live: true };
const restart = { key: 'cpu', label: 'CPU overcommit', live: false };

const meta = {
  title: 'Blocks/SaveBar',
  component: SaveBar,
  args: { changes: [live], saving: false, onDiscard: () => {}, onSave: () => {} },
  render: (args) => <div className="w-popover font-sans"><SaveBar {...args} /></div>,
} satisfies Meta<typeof SaveBar>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Live: Story = {};
export const OnRestart: Story = { args: { changes: [restart] } };
export const Mixed: Story = { args: { changes: [live, restart] } };
export const AndMore: Story = { args: { changes: [live, restart, { key: 'a', label: 'Margin', live: true }, { key: 'b', label: 'Reserve', live: true }] } };
export const Saving: Story = { args: { saving: true } };
export const Summary: Story = { args: { summary: '2 lines changed. Headroom applies now, sweep interval on restart.' } };
