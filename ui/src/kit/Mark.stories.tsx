import type { Meta, StoryObj } from '@storybook/react-vite';
import { Mark } from './Mark';

const meta = { title: 'Kit/Mark', component: Mark } satisfies Meta<typeof Mark>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Logo: Story = {
  render: () => (
    <span className="flex items-center gap-sm">
      <Mark variant="logo" />
      <span className="text-title font-semibold text-fg-strong">faber</span>
    </span>
  ),
};

export const Offline: Story = {
  render: () => (
    <span className="flex rounded-full bg-surface-2 p-lg text-muted">
      <Mark variant="offline" />
    </span>
  ),
};

export const Sizes: Story = {
  render: () => (
    <>
      <Mark size="sm" />
      <Mark size="md" />
      <Mark size="lg" />
    </>
  ),
};
