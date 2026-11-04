import type { Meta, StoryObj } from '@storybook/react-vite';
import { Download } from 'lucide-react';
import { Icon } from './Icon';
import { RowButton } from './RowButton';

// RowButton's @states on the popover card's width: a full-bleed line, resting, hovered, pressed
// and with the keyboard ring.

const meta = {
  title: 'Kit/RowButton',
  component: RowButton,
  args: {
    onClick: () => {},
    children: (
      <>
        <Icon icon={Download} size="sm" />
        Open panel
      </>
    ),
  },
  render: (args) => (
    <div className="flex w-popover flex-col rounded-lg border border-border bg-bg font-sans text-body text-fg">
      <RowButton {...args} />
    </div>
  ),
} satisfies Meta<typeof RowButton>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'button' } } };
export const Pressed: Story = { parameters: { force: { pressed: 'button' } } };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'button' } } };
