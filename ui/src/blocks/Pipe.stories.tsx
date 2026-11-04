import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pipe } from './Pipe';

const meta = { title: 'Blocks/Pipe', component: Pipe } satisfies Meta<typeof Pipe>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {
  render: () => (
    <p className="text-body font-semibold text-fg-strong">
      site
      <Pipe />
      Deploy
    </p>
  ),
};
