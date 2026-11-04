import type { Meta, StoryObj } from '@storybook/react-vite';
import { UpdateCard } from './UpdateCard';

// UpdateCard's @states (default, installing, failed, no skip, with details link).

const meta = {
  title: 'Blocks/UpdateCard',
  component: UpdateCard,
  args: {
    version: '0.3.5',
    body: 'Installs when this host goes idle.',
    onInstall: () => {},
    onSkip: () => {},
  },
  render: (args) => (
    <div className="w-sidebar bg-surface-2 p-lg font-sans text-body text-fg">
      <UpdateCard {...args} />
    </div>
  ),
} satisfies Meta<typeof UpdateCard>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const Installing: Story = { args: { pending: 'install' } };
export const Failed: Story = { args: { error: 'The download failed: no route to host.' } };
export const NoSkip: Story = { args: { onSkip: undefined } };
export const WithDetails: Story = { args: { href: '/maintenance' } };
