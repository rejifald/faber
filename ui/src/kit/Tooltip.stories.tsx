import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pause, RefreshCw, Trash2 } from 'lucide-react';
import { hover } from '../../.storybook/play';
import { Button } from './Button';
import { IconButton } from './IconButton';
import { Tooltip } from './Tooltip';

// Tooltip's @states (closed, open) on each side, and TooltipProvider's shared delay. Every story
// already renders inside one TooltipProvider, as an entry does. The default previews why a live
// button cannot act now; the press itself says so too (Button's Unavailable stories).

const meta = {
  title: 'Kit/Tooltip',
  component: Tooltip,
  args: {
    content: 'No jobs are running',
    children: <Button icon={Pause}>Pause</Button>,
  },
} satisfies Meta<typeof Tooltip>;
export default meta;
type Story = StoryObj<typeof meta>;

const show = (context: { canvasElement: HTMLElement }) => hover(context, 'button');

export const Closed: Story = {};

export const Open: Story = { play: show };

export const OpenBottom: Story = { args: { side: 'bottom' }, play: show };

export const OpenRight: Story = { args: { side: 'right' }, play: show };

/**
 * TooltipProvider: a row sharing the one provider the story browser mounts, as an entry does.
 * Once a tooltip shows, moving along the row shows the next at once.
 */
export const ProviderRow: Story = {
  render: () => (
    <div className="flex gap-2xs">
      <IconButton icon={RefreshCw} label="Reconcile now" />
      <IconButton icon={Pause} label="Pause the host" />
      <IconButton icon={Trash2} label="Clear finished jobs" />
    </div>
  ),
};
