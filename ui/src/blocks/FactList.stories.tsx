import type { Meta, StoryObj } from '@storybook/react-vite';
import { StoryCard } from '../popover/StoryCard';
import { FactList } from './FactList';

const meta = {
  title: 'Blocks/FactList',
  component: FactList,
  args: {
    facts: [
      { label: 'CPU', value: '6 vCPU' },
      { label: 'Memory', value: '16 GiB' },
      { label: 'Disk free', value: '40 GiB' },
    ],
  },
  render: (args) => (
    <StoryCard inset={false}>
      <div className="w-gauge-col pb-2xs">
        <FactList {...args} />
      </div>
    </StoryCard>
  ),
} satisfies Meta<typeof FactList>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const WithAWordValue: Story = {
  args: { facts: [{ label: 'State', value: 'proving the wall', word: true }, { label: 'CPU', value: '4 vCPU' }] },
};
export const LongLabel: Story = {
  args: { facts: [{ label: 'Reserved for jobs already running on this host', value: '6.0 GiB' }] },
};
