import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { Disclosure } from './Disclosure';

// Disclosure's @states (closed, hover, open, focus-visible), drawn as the popover uses it.

function Example({ open: initial = false }: { open?: boolean }) {
  const [open, setOpen] = useState(initial);
  return (
    <div className="w-popover rounded-lg border border-border bg-bg py-xs">
      <Disclosure
        label="demo/PR-2741, 3 running, 6 minutes"
        title="feature/cache"
        open={open}
        onOpenChange={setOpen}
        summary={
          <span className="flex items-center gap-sm">
            <span className="flex-1 font-semibold text-fg-strong">demo/PR-2741</span>
            <span className="font-mono text-caption text-faint">6m</span>
          </span>
        }
      >
        <p className="py-3xs pr-xl pl-4xl text-body text-muted">api e2e</p>
        <p className="py-3xs pr-xl pl-4xl text-body text-muted">web unit</p>
      </Disclosure>
    </div>
  );
}

const meta = {
  title: 'Kit/Disclosure',
  component: Disclosure,
  args: { summary: null, label: '', open: false, onOpenChange: () => {}, children: null },
  render: () => <Example />,
} satisfies Meta<typeof Disclosure>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Closed: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'button' } } };
export const Pressed: Story = { parameters: { force: { pressed: 'button' } } };
export const Open: Story = { render: () => <Example open /> };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'button' } } };
