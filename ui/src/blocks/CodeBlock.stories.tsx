import type { Meta, StoryObj } from '@storybook/react-vite';
import { CodeBlock } from './CodeBlock';

// CodeBlock's @states (default, long lines): a message in an opened Activity row.

const meta = {
  title: 'Blocks/CodeBlock',
  component: CodeBlock,
  decorators: [(Story) => <div className="w-rail"><Story /></div>],
} satisfies Meta<typeof CodeBlock>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {
  args: { children: 'Runner lost communication with the server.\nThe job was queued again.' },
};

export const LongLines: Story = {
  args: {
    children:
      'error: failed to push some refs to https://example.invalid/acme/web-with-a-very-long-repository-name-that-keeps-going/branches/feature-abcdefghijklmnopqrstuvwxyz0123456789',
  },
};
