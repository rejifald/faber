import type { Meta, StoryObj } from '@storybook/react-vite';
import { BudgetRail } from './BudgetRail';

// BudgetRail's @states. Default: 12.0 GiB reserved of 18.0 (67 %), 2.0 reserve, 4.0 free.

const jobs = [
  { key: 'e2e', label: 'acme/web e2e', mib: 6144 },
  { key: 'build', label: 'acme/mobile build', mib: 3072 },
  { key: 'test', label: 'acme/web test', mib: 2048 },
  { key: 'lint', label: 'acme/docs lint', mib: 1024 },
];
const base = { budgetMib: 18432, reserveMib: 2048, jobs, admitted: 3, parked: 1, safetyMargin: 1.15 };

const meta = { title: 'Blocks/BudgetRail', component: BudgetRail, args: base, parameters: { layout: 'padded' } } satisfies Meta<typeof BudgetRail>;
export default meta;
type Story = StoryObj<typeof meta>;

const card = (props: Parameters<typeof BudgetRail>[0]) => <BudgetRail {...props} />;

export const Default: Story = { render: () => card(base) };
export const NoJobs: Story = { render: () => card({ ...base, jobs: [], admitted: 0, parked: 0 }) };
/** Jobs and reserve exceed the budget: free is absent and the reserved figure is amber. */
export const OverBudget: Story = {
  render: () =>
    card({
      ...base,
      jobs: [
        { key: 'e2e', label: 'acme/web e2e', mib: 9216 },
        { key: 'build', label: 'acme/mobile build', mib: 7168 },
        { key: 'test', label: 'acme/web test', mib: 3072 },
      ],
      admitted: 3,
      parked: 2,
    }),
};
/** Twelve jobs under 1 % of the rail merge into one "12 more" segment and legend entry. */
export const ManyJobs: Story = {
  render: () =>
    card({
      ...base,
      jobs: [
        ...jobs,
        ...Array.from({ length: 12 }, (_, i) => ({ key: `tiny-${i}`, label: `acme/shard ${i + 1}`, mib: 128 })),
      ],
      admitted: 16,
    }),
};
