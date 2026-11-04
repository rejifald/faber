import type { Meta, StoryObj } from '@storybook/react-vite';
import { NOW, fault, job, verdict } from '../popover/fixtures';
import { buildGroups } from '../popover/groups';
import { StoryCard } from '../popover/StoryCard';
import type { DaemonState } from '../popover/types';
import { CompactGroup } from './CompactGroup';

// CompactGroup's @states: a group folded to one line, its one pip the worst state present.

const groupOf = (s: DaemonState) => buildGroups(s, new Map(), NOW)[0];

const meta = {
  title: 'Blocks/CompactGroup',
  component: CompactGroup,
  args: { group: groupOf({ inflight: [job({ pr: 2770, elapsed_s: 95 })] }) },
  render: (args) => (
    <StoryCard>
      <CompactGroup {...args} />
    </StoryCard>
  ),
} satisfies Meta<typeof CompactGroup>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Running: Story = {};
/** Running and waiting jobs: the waiting clock wins. */
export const Waiting: Story = {
  args: {
    group: groupOf({
      inflight: [
        job({ pr: 2771, elapsed_s: 95 }),
        job({ pr: 2771, admitted: false, step: null, waiting: { gate: 'memory', detail: 'needs 3072 MiB' }, elapsed_s: 12 }),
      ],
    }),
  },
};
export const Fault: Story = { args: { group: groupOf({ outcomes: [fault({ pr: 2774, ts_ms: NOW - 45_000 })] }) } };
export const Frozen: Story = {
  args: {
    group: groupOf({
      inflight: [job({ pr: 2772, elapsed_s: 40, frozen: { since_ms: NOW - 5000, reason: 'disk', detail: 'free space under the floor' } })],
    }),
  },
};
/** A repo and workflow group whose only job just finished. Held still, as between snapshots. */
export const Done: Story = {
  parameters: { still: true },
  args: { group: groupOf({ outcomes: [verdict('success', { pr: null, workflow: 'Deploy', job_name: 'deploy', ts_ms: NOW - 4000 })] }) },
};
