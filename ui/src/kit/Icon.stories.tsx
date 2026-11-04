import type { Meta, StoryObj } from '@storybook/react-vite';
import { TriangleAlert } from 'lucide-react';
import { Icon } from './Icon';
import type { Size } from './ladder';

// Icon's one state at each ladder step, beside text of that step's label size.

const meta = { title: 'Kit/Icon', component: Icon, args: { icon: TriangleAlert } } satisfies Meta<typeof Icon>;
export default meta;
type Story = StoryObj<typeof meta>;

const LABEL: Record<Size, string> = { sm: 'text-caption', md: 'text-small', lg: 'text-body' };

export const Default: Story = {
  render: (args) => (
    <div className="flex flex-col gap-sm text-amber">
      {(['sm', 'md', 'lg'] as Size[]).map((size) => (
        <span key={size} className={`flex items-center gap-xs ${LABEL[size]}`}>
          <Icon {...args} size={size} />
          {`Docker admission held (${size})`}
        </span>
      ))}
      <span className="flex w-popover gap-sm text-small text-fg">
        <span className="text-amber">
          <Icon {...args} leading />
        </span>
        A leading icon sits on the first line of text that wraps onto a second line, like an attention row.
      </span>
    </div>
  ),
};
