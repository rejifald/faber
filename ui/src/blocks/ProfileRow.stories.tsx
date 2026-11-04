import type { Meta, StoryObj } from '@storybook/react-vite';
import { ProfileRow } from './ProfileRow';

// ProfileRow as docs/design/settings/anatomy.md describes it.

const meta = {
  title: 'Blocks/ProfileRow',
  component: ProfileRow,
  args: { order: 1, name: 'linux-docker', labels: ['faber', 'docker'], facts: ['docker', 'Docker access'], onOpen: () => {} },
  render: (args) => (
    <div className="bg-surface p-xl font-sans text-body text-fg">
      <ProfileRow {...args} />
    </div>
  ),
} satisfies Meta<typeof ProfileRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'div' } } };
export const Focused: Story = { parameters: { force: { 'focus-visible': 'button' } } };
export const Vm: Story = { args: { order: 3, name: 'macos', labels: ['faber', 'macos'], facts: ['macos-vm', '4 vCPU', '8 GiB'] } };
