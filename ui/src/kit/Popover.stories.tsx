import type { Meta, StoryObj } from '@storybook/react-vite';
import { click } from '../../.storybook/play';
import { Popover } from './Popover';

// Popover's @states (closed, hover, focus-visible, open). A popup opens in a play function.

const trigger = 'button[aria-haspopup="dialog"]';

const meta = {
  title: 'Kit/Popover',
  component: Popover,
  args: { trigger: 'Details', title: 'Last build', children: null },
  render: (args) => (
    <Popover {...args}>
      <p className="px-sm py-2xs text-small text-fg">Finished 12 s ago on alpha.</p>
    </Popover>
  ),
} satisfies Meta<typeof Popover>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Closed: Story = {};
export const Hover: Story = { parameters: { force: { hover: trigger } } };
export const Focused: Story = { parameters: { force: { 'focus-visible': trigger } } };
export const Open: Story = { play: (context) => click(context, trigger, '[role="dialog"]') };
