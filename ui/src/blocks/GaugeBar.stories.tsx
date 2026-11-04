import type { Meta, StoryObj } from '@storybook/react-vite';
import { budget } from '../popover/fixtures';
import { gaugeModels, type GaugeModel } from '../popover/gauges';
import { StoryCard } from '../popover/StoryCard';
import { GaugeBar } from './GaugeBar';

// GaugeBar's @states, from the same model the screen builds (popover/gauges.ts), in the 240 px
// gauge column's inset.

const meta = { title: 'Blocks/GaugeBar', component: GaugeBar } satisfies Meta<typeof GaugeBar>;
export default meta;
type Story = StoryObj<typeof meta>;

const busy = budget({ docker_inflight: 3, reserved_mib: 5632, cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 3000 } });
const gauge = (key: GaugeModel['key'], gate: string | null = null, parked = false, b = busy) =>
  gaugeModels(b, gate, parked).find((g) => g.key === key)!;

const column = (...gauges: GaugeModel[]) => (
  <StoryCard>
    <div className="flex w-gauge-col flex-col px-xl py-lg">
      {gauges.map((g) => (
        <GaugeBar key={`${g.key}-${g.caption}`} gauge={g} />
      ))}
    </div>
  </StoryCard>
);

/** Memory, CPU and disk, none binding: each with its fit mark. */
export const Default: Story = { render: () => column(gauge('mem'), gauge('cpu'), gauge('disk')) };
/** Memory binds: its name marked "binds", and the bar in the accent. */
export const Binding: Story = { render: () => column(gauge('mem', 'memory')) };
/** Memory sized at p75 binds, with a double-digit figure: the name gives way, "binds" stays. */
export const BindingLongCaption: Story = {
  render: () =>
    column(
      gauge('mem', 'memory', false, budget({ budget_mib: 16384, reserved_mib: 14336, reserve: { in_force: 'p75' } })),
    ),
};
/** Memory binds while claims wait at it: amber. */
export const BindingWhileWaiting: Story = { render: () => column(gauge('mem', 'memory', true)) };
/** CPU not in `auto`: faint, its figure ending "not gated". */
export const NotGated: Story = {
  render: () => column(gauge('cpu', null, false, budget({ cpu: { mode: 'off', budget_mcpu: 8000, reserved_mcpu: 3000 } }))),
};
/** Slots: one segment per slot, three of six running. */
export const Slots: Story = { render: () => column(gauge('slots')) };
/** Slots binding while something waits. */
export const SlotsBinding: Story = { render: () => column(gauge('slots', 'ceiling', true)) };
/** An older daemon without headroom: no fit mark. */
export const NoFitMark: Story = {
  render: () => column(gauge('mem', null, false, budget({ headroom: null, reserved_mib: 4000 }))),
};
/** The loading frame: empty tracks under the captions. */
export const Loading: Story = {
  render: () => (
    <StoryCard>
      <div className="flex w-gauge-col flex-col px-xl py-lg">
        {['Memory', 'CPU', 'Disk'].map((p) => (
          <GaugeBar key={p} placeholder={p} />
        ))}
      </div>
    </StoryCard>
  ),
};
