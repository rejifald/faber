import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { SegmentedControl } from '../kit';
import { Panel } from './Panel';

// Panel as the Overview draws In flight and Activity (dashboard/anatomy.md § 5).

const meta = { title: 'Blocks/Panel', component: Panel, args: { title: 'In flight' }, parameters: { layout: 'padded' } } satisfies Meta<typeof Panel>;
export default meta;
type Story = StoryObj<typeof meta>;

const rows = (
  <ul className="divide-y divide-border-soft text-body">
    <li className="px-xl py-md">acme/web build</li>
    <li className="px-xl py-md">acme/api test</li>
    <li className="px-xl py-md">acme/web lint</li>
  </ul>
);

export const Default: Story = { args: { children: rows } };
export const WithMeta: Story = { args: { meta: '3 admitted, 1 parked', children: rows } };

function Filtered() {
  const [value, setValue] = useState('all');
  return (
    <Panel
      title="Activity"
      action={
        <SegmentedControl
          label="Filter activity"
          value={value}
          onValueChange={setValue}
          items={[
            { value: 'all', label: 'All' },
            { value: 'delivered', label: 'Delivered' },
            { value: 'faults', label: 'Faults' },
          ]}
        />
      }
    >
      {rows}
    </Panel>
  );
}
export const WithAction: Story = { args: { children: null }, render: () => <Filtered /> };

export const Bare: Story = {
  args: {
    bare: true,
    children: (
      <div className="flex flex-col gap-sm">
        <div className="rounded-lg border border-border bg-surface px-xl py-lg text-body">acme/web needs a label</div>
        <div className="rounded-lg border border-border bg-surface px-xl py-lg text-body">acme/api is parked</div>
      </div>
    ),
  },
};

export const Empty: Story = {
  args: { children: <p className="px-xl py-lg text-body text-muted">Nothing is running. Jobs appear here as they are admitted.</p> },
};
