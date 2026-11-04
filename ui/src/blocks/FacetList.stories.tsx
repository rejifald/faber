import { useState, type ComponentProps } from 'react';
import type { Meta, StoryObj } from '@storybook/react-vite';
import { FacetList, type FacetGroup } from './FacetList';

// FacetList's @states (default, item hover, item selected, focus-visible, with dots, empty
// group hidden): Activity's filter column.

const groups: FacetGroup[] = [
  {
    id: 'kind',
    label: 'Kind',
    items: [
      { value: 'job', label: 'Job', count: 412 },
      { value: 'runner', label: 'Runner', count: 96 },
      { value: 'gate', label: 'Gate', count: 23 },
    ],
  },
  {
    id: 'repo',
    label: 'Repo',
    items: [
      { value: 'acme/web', label: 'acme/web', count: 240 },
      { value: 'acme/api', label: 'acme/api', count: 131 },
      { value: 'owner/demo', label: 'owner/demo', count: 41 },
    ],
  },
];

function Controlled(args: ComponentProps<typeof FacetList>) {
  const [selected, setSelected] = useState<Record<string, string[]>>({ kind: ['job', 'gate'], repo: ['acme/web'] });
  return <FacetList {...args} selected={selected} onChange={(id, v) => setSelected({ ...selected, [id]: v })} />;
}

const meta = {
  title: 'Blocks/FacetList',
  component: FacetList,
  args: { groups, selected: {}, onChange: () => {} },
  decorators: [(Story) => <div className="w-rail"><Story /></div>],
} satisfies Meta<typeof FacetList>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

/** Any number can be chosen in a group; the count on a chosen item stays readable. */
export const Selected: Story = {
  render: (args) => <Controlled {...args} />,
};

export const WithDots: Story = {
  args: {
    groups: [
      {
        id: 'result',
        label: 'Result',
        items: [
          { value: 'delivered', label: 'Delivered', count: 388, tone: 'green' },
          { value: 'timed-out', label: 'Timed out', count: 5, tone: 'amber' },
          { value: 'refused', label: 'Refused', count: 23, tone: 'red' },
        ],
      },
    ],
    selected: { result: ['refused'] },
  },
};

export const EmptyGroupHidden: Story = {
  args: { groups: [...groups, { id: 'actor', label: 'Actor', items: [] }] },
};
