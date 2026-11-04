import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pause, RotateCcw } from 'lucide-react';
import { Button, SplitButton } from '../kit';
import { PageHeader } from './PageHeader';

// PageHeader as the Overview draws it (dashboard/anatomy.md § 5).

const meta = {
  title: 'Blocks/PageHeader',
  component: PageHeader,
  args: { title: 'Overview' },
  parameters: { layout: 'padded' },
} satisfies Meta<typeof PageHeader>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const Live: Story = { args: { live: true } };
export const Stale: Story = { args: { live: 'stale' } };

export const WithMeta: Story = { args: { title: 'Activity', live: true, meta: '17 events' } };

export const WithActions: Story = {
  args: {
    live: true,
    actions: (
      <>
        <Button variant="secondary" size="lg" icon={RotateCcw}>
          Reconcile
        </Button>
        <SplitButton icon={Pause} menuLabel="More pause options" items={[{ label: 'Drain', onSelect: () => {} }]}>
          Pause
        </SplitButton>
      </>
    ),
  },
};
