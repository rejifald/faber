import type { Meta, StoryObj } from '@storybook/react-vite';
import type { AttentionRow } from '../popover/attention';
import { StoryCard } from '../popover/StoryCard';
import { DismissedList } from './DismissedList';

// DismissedList's @states (collapsed, open, restore pending, restore failed, in a card), on the
// card under the attention list, as the popover places it; in a card, as the dashboard does.

const rows: AttentionRow[] = [
  {
    key: 'oom-held',
    sev: 'bad',
    icon: 'alert',
    parts: [{ strong: 'OOM-killed on 5 of its last 10 runs' }, ': owner/demo', { sep: true }, 'CI', { sep: true }, 'test — held at 8.5 GiB.'],
  },
  { key: 'bypass', sev: 'warn', icon: 'alert', parts: [{ strong: '2 trust bypasses in 7 days' }, ' — enforcement off.'] },
];

const meta = {
  title: 'Blocks/DismissedList',
  component: DismissedList,
  args: { rows, open: false, onOpenChange: () => {}, onRestore: () => {} },
  render: (args) => (
    <StoryCard inset={false}>
      <DismissedList {...args} />
    </StoryCard>
  ),
} satisfies Meta<typeof DismissedList>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Collapsed: Story = {};
export const Open: Story = { args: { open: true } };
/** The pressed Restore shows loading; the others wait until it settles. */
export const RestorePending: Story = { args: { open: true, pending: 'oom-held' } };
export const RestoreFailed: Story = { args: { open: true, error: 'faber answered 503' } };
/** On the dashboard (Overview's rail, Memory's warnings) it stands alone, in a card of its own. */
export const InACard: Story = {
  args: { open: true, card: true },
  render: (args) => (
    <div className="w-popover bg-bg p-xl">
      <DismissedList {...args} />
    </div>
  ),
};
