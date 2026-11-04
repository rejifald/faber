import type { Meta, StoryObj } from '@storybook/react-vite';
import { StatusBadge } from './StatusBadge';

// One story per state in StatusBadge's @states: each tone, and a long label truncated.

const meta = {
  title: 'Kit/StatusBadge',
  component: StatusBadge,
  args: { tone: 'green', children: 'Delivered' },
} satisfies Meta<typeof StatusBadge>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Green: Story = {};
export const Amber: Story = { args: { tone: 'amber', children: 'Timed out' } };
export const Red: Story = { args: { tone: 'red', children: 'Infra error' } };
export const Neutral: Story = { args: { tone: 'neutral', children: 'Refused' } };
export const Accent: Story = { args: { tone: 'accent', children: 'Pinned' } };

/** A label longer than its container is cut with an ellipsis. */
export const Truncated: Story = {
  render: () => (
    <div className="w-4xl">
      <StatusBadge tone="amber">Timed out waiting for a runner</StatusBadge>
    </div>
  ),
};
