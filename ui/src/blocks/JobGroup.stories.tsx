import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { busy, busyOverflow, finished, NOW, pausedFrozen, waiting } from '../popover/fixtures';
import { buildGroups, type Group } from '../popover/groups';
import type { Snapshot } from '../popover/types';
import { StoryCard } from '../popover/StoryCard';
import { JobGroup } from './JobGroup';

// JobGroup's @states, each built from a fixture by the screen's own grouping (popover/groups.ts).

const groupsOf = (snap: Snapshot) => buildGroups(snap.state ?? {}, new Map(), NOW);
const find = (snap: Snapshot, name: string) => groupsOf(snap).find((g) => g.name === name)!;

function Row({ group, open = false }: { group: Group; open?: boolean }) {
  const [isOpen, setOpen] = useState(open);
  return <JobGroup group={group} open={isOpen} onOpenChange={setOpen} />;
}

const meta = {
  title: 'Blocks/JobGroup',
  component: JobGroup,
  args: { group: find(busy, 'demo/PR-2741'), open: false, onOpenChange: () => {} },
  render: (args) => (
    <StoryCard>
      <Row group={args.group} open={args.open} />
    </StoryCard>
  ),
} satisfies Meta<typeof JobGroup>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Running: Story = { args: { group: find(busy, 'demo/PR-2741') } };
/** A faber fault beside a running job: the red ring comes first. */
export const Mixed: Story = { args: { group: find(busy, 'demo/PR-2738') } };
/** All waiting: no note, the clock pips say it waits. */
export const AllWaiting: Story = { args: { group: find(waiting, 'site') } };
/** Open, each waiting job's line says why. */
export const AllWaitingOpen: Story = { args: { group: find(waiting, 'site'), open: true } };

export const Frozen: Story = { args: { group: find(pausedFrozen, 'demo/PR-2741') } };
/** One job passed and one failed beside the running ones: both are done, the same muted pip, first. */
export const Done: Story = { parameters: { still: true }, args: { group: find(finished, 'demo/PR-2760') } };
/** The open group lists each finished job as done; CI's own verdict is only in its hover title. */
export const DoneOpen: Story = { parameters: { still: true }, args: { group: find(finished, 'demo/PR-2760'), open: true } };
/** Only a done job is left: the row stays while its pip lingers, with no note. */
export const DoneOnly: Story = { parameters: { still: true }, args: { group: find(finished, 'demo/PR-2761') } };
/** CI failed in two jobs: not faber's to report, so no red line, only two done pips. */
export const CiFailedIsDone: Story = { parameters: { still: true }, args: { group: find(finished, 'demo/PR-2763') } };
/** Only a lingering fault is left: the row stays, red line naming the job, age since it ended. */
export const FaultOnly: Story = { args: { group: find(finished, 'demo/PR-2764') } };
export const OverflowBar: Story = { args: { group: find(busyOverflow, 'demo/PR-2750') } };
export const Hover: Story = { parameters: { force: { hover: 'button' } }, args: { group: find(busy, 'demo/PR-2738') } };
export const Open: Story = { args: { group: find(busy, 'demo/PR-2741'), open: true } };
