import type { Meta, StoryObj } from '@storybook/react-vite';
import { click } from '../../.storybook/play';
import type { PeerRowModel } from '../popover/peers';
import { HostCard } from './HostCard';

// HostCard's @states (default, no peers, with stale peer, popover open, hover, focus-visible).

const fresh: PeerRowModel = {
  key: 'a1',
  name: 'peer beta',
  runners: 2,
  pips: [
    { kind: 'running', key: 'a1-0' },
    { kind: 'running', key: 'a1-1' },
  ],
  stale: false,
  age: '12s',
  label: 'peer beta, 2 runners, seen 12 seconds ago',
};
const stale: PeerRowModel = {
  key: 'b2',
  name: 'peer builder',
  runners: 1,
  pips: [{ kind: 'waiting', key: 'b2-0' }],
  stale: true,
  age: '9m',
  label: 'peer builder, 1 runner, seen 9 minutes ago, stale',
};

const meta = {
  title: 'Blocks/HostCard',
  component: HostCard,
  args: { name: 'alpha', tone: 'green', summary: '2 hosts, both healthy', peers: [fresh] },
  render: (args) => (
    <div className="w-sidebar bg-surface-2 p-lg font-sans text-body text-fg">
      <HostCard {...args} />
    </div>
  ),
} satisfies Meta<typeof HostCard>;
export default meta;
type Story = StoryObj<typeof meta>;

const trigger = 'button[aria-haspopup="dialog"]'; // the whole card, named by its own text

export const Default: Story = {};
export const NoPeers: Story = { args: { summary: '1 host', peers: [] } };
export const WithStalePeer: Story = {
  args: { tone: 'amber', summary: '2 hosts, one not seen', peers: [fresh, stale] },
  play: (context) => click(context, trigger, '[role="dialog"]'),
};
export const PopoverOpen: Story = { play: (context) => click(context, trigger, '[role="dialog"]') };
export const Hover: Story = { parameters: { force: { hover: trigger } } };
export const Focused: Story = { parameters: { force: { 'focus-visible': trigger } } };
