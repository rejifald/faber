import type { Meta, StoryObj } from '@storybook/react-vite';
import { ConfirmRow } from './ConfirmRow';

// ConfirmRow as docs/design/settings/anatomy.md describes it.

const meta = {
  title: 'Blocks/ConfirmRow',
  component: ConfirmRow,
  args: { question: 'Remove linux?', consequence: 'Jobs for faber, linux go to the next profile that matches, or wait if none does.', onCancel: () => {}, onConfirm: () => {} },
} satisfies Meta<typeof ConfirmRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = { render: (args) => <div className="w-popover font-sans"><ConfirmRow {...args} /></div> };
