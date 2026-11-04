import type { Meta, StoryObj } from '@storybook/react-vite';
import { StoryCard } from '../popover/StoryCard';
import { MemoryBar } from './MemoryBar';

// MemoryBar's @states, with the strip's numbers (settings/anatomy.md § 5): 2.0 GiB reserve,
// 6.0 jobs, 1.0 headroom and 9.0 free of 18.0 GiB.

const meta = { title: 'Blocks/MemoryBar', component: MemoryBar, args: { totalMib: 18432, reserveMib: 2048, jobsMib: 6144, headroomMib: 1024 } } satisfies Meta<typeof MemoryBar>;
export default meta;
type Story = StoryObj<typeof meta>;

const card = (props: Parameters<typeof MemoryBar>[0]) => (
  <StoryCard inset={false}>
    <MemoryBar {...props} />
  </StoryCard>
);
const base = { totalMib: 18432, reserveMib: 2048, jobsMib: 6144, headroomMib: 1024 };

export const Default: Story = { render: () => card(base) };
export const NoJobs: Story = { render: () => card({ ...base, jobsMib: 0 }) };
/** Jobs and headroom use the whole budget, so Free is absent. */
export const Full: Story = { render: () => card({ ...base, jobsMib: 15360 }) };
