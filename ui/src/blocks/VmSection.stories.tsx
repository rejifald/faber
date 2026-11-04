import type { Meta, StoryObj } from '@storybook/react-vite';
import * as fx from '../popover/fixtures';
import { StoryCard } from '../popover/StoryCard';
import { vmSection } from '../popover/vms';
import { VmSection } from './VmSection';

// VmSection's @states, from the fixtures' budgets, in the 240 px left column.

const from = (snap: typeof fx.running) => vmSection(snap.state?.budget, fx.NOW);

const meta = {
  title: 'Blocks/VmSection',
  component: VmSection,
  args: from(fx.running),
  render: (args) => (
    <StoryCard inset={false}>
      <div className="w-gauge-col">
        <VmSection {...args} />
      </div>
    </StoryCard>
  ),
} satisfies Meta<typeof VmSection>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Three rows. */
export const Default: Story = {};
/** The Linux VM open. */
export const RowOpen: Story = { args: { openKey: 'linux-0' } };
/** An idle popover's one Linux VM. */
export const OneRow: Story = { args: from(fx.idleVm) };
/** The supervisor is unreachable: `as of 3m ago` on the label, every row neutral. */
export const Stale: Story = { args: from(fx.vmsStale) };
/** An older daemon sends no sizes: no row opens. */
export const NoSizes: Story = {
  args: {
    ...from(fx.running),
    rows: from(fx.running).rows.map((r) => ({ ...r, facts: [] })),
  },
};
