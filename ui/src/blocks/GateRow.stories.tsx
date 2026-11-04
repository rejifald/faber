import type { Meta, StoryObj } from '@storybook/react-vite';
import { budget } from '../popover/fixtures';
import { gateFacts, gaugeModels } from '../popover/gauges';
import { StoryCard } from '../popover/StoryCard';
import { GateRow } from './GateRow';

// GateRow's @states, from the models the screen builds (popover/gauges.ts), in the 240 px gauge column.

const busy = budget({ docker_inflight: 3, reserved_mib: 5632, reserve: { in_force: 'max' } });
const parked = budget({ docker_inflight: 2, reserved_mib: 5632, gates: { memory: { parked_now: 3 } } });
const mem = (b = busy, waits = false) => gaugeModels(b, 'memory', waits).find((g) => g.key === 'mem')!;

const meta = {
  title: 'Blocks/GateRow',
  component: GateRow,
  args: { gauge: mem(), facts: gateFacts(mem(), busy), open: false, onOpenChange: () => {}, onAdjust: () => {} },
  render: (args) => (
    <StoryCard>
      <div className="w-gauge-col py-xs">
        <GateRow {...args} />
      </div>
    </StoryCard>
  ),
} satisfies Meta<typeof GateRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Closed: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'button[aria-label^="Memory"]' } } };
export const Open: Story = { args: { open: true } };
/** Claims parked at memory: no "Room for", and the amber wait line last. */
export const OpenWhileJobsWait: Story = { args: { open: true, gauge: mem(parked, true), facts: gateFacts(mem(parked, true), parked) } };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'button[aria-label^="Memory"]' } } };
