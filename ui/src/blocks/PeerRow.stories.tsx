import type { Meta, StoryObj } from '@storybook/react-vite';
import { NOW } from '../popover/fixtures';
import { peerRows } from '../popover/peers';
import { StoryCard } from '../popover/StoryCard';
import { PeerRow } from './PeerRow';

const peer = (seenAgoS: number, runners = 2, busy: number | null = runners) =>
  peerRows(
    [
      {
        instance: 'c3d4',
        last_seen_ms: NOW - seenAgoS * 1000,
        runners: { 'owner/demo': runners },
        // null: an older daemon, which sends no busy or idle count.
        ...(busy === null ? {} : { busy: { 'owner/demo': busy }, idle: { 'owner/demo': runners - busy } }),
      },
    ],
    NOW,
  )[0];

const meta = {
  title: 'Blocks/PeerRow',
  component: PeerRow,
  args: { peer: peer(40) },
  render: (args) => (
    <StoryCard>
      <PeerRow {...args} />
    </StoryCard>
  ),
} satisfies Meta<typeof PeerRow>;
export default meta;
type Story = StoryObj<typeof meta>;

/** One busy runner, one idle. */
export const Fresh: Story = { args: { peer: peer(40, 2, 1) } };
export const AllIdle: Story = { args: { peer: peer(40, 3, 0) } };
export const Stale: Story = { args: { peer: peer(7 * 60) } };
export const ManyRunners: Story = { args: { peer: peer(40, 9, 6) } };
/** An older daemon sends only the count: every runner idle, claiming no work. */
export const OlderDaemon: Story = { args: { peer: peer(40, 2, null) } };
export const NoRunners: Story = { args: { peer: peer(40, 0) } };
/** The popup list's row, as in the dashboard HostCard. */
export const Dense: Story = { args: { dense: true } };
