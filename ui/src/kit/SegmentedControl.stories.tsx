import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import type { ForcedState } from '../../.storybook/force';
import { SegmentedControl, type SegmentedControlProps, type SegmentedItem } from './SegmentedControl';
import type { Size } from './ladder';

// One story per state in SegmentedControl's @states, plus the ladder steps. Hover and keyboard
// focus land on one choice beside the selected one, which rests.

const meta = {
  title: 'Kit/SegmentedControl',
  component: SegmentedControl,
  args: { label: 'Filter activity', value: 'all', items: [], onValueChange: () => {} },
} satisfies Meta<typeof SegmentedControl>;
export default meta;
type Story = StoryObj<typeof meta>;

const PLAIN: SegmentedItem[] = [
  { value: 'all', label: 'All' },
  { value: 'delivered', label: 'Delivered' },
  { value: 'faults', label: 'Faults' },
  { value: 'refused', label: 'Refused' },
  { value: 'retried', label: 'Retried' },
];

const COUNTED: SegmentedItem[] = [
  { value: 'all', label: 'All', count: 412 },
  { value: 'delivered', label: 'Delivered', count: 388 },
  { value: 'faults', label: 'Faults', count: 7 },
  { value: 'refused', label: 'Refused', count: 9 },
  { value: 'retried', label: 'Retried', count: 8 },
];

const TONED: SegmentedItem[] = COUNTED.map((item) =>
  item.value === 'faults' ? { ...item, tone: 'amber' as const } : item,
);

/** Owns the selection the way a screen would. */
function Live({ initial = 'all', ...props }: Omit<SegmentedControlProps, 'value' | 'onValueChange' | 'label'> & { initial?: string }) {
  const [value, setValue] = useState(initial);
  return <SegmentedControl label="Filter activity" value={value} onValueChange={setValue} {...props} />;
}

const forced = (state: ForcedState, selector: string): Story => ({
  parameters: { force: { [state]: selector } },
  render: () => <Live items={PLAIN} />,
});

export const Default: Story = { render: () => <Live items={PLAIN} /> };

export const Hover = forced('hover', '[role="group"] > button:not([data-pressed])');

/** The selected choice rests on a raised surface, here one past the first; pressing another moves it. */
export const Selected: Story = { render: () => <Live items={PLAIN} initial="faults" /> };

export const FocusVisible = forced('focus-visible', '[role="group"] > button:not([data-pressed])');

export const WithCounts: Story = { render: () => <Live items={COUNTED} /> };

/** A choice that needs attention takes the tone's colour on its label and count. */
export const ToneItem: Story = { render: () => <Live items={TONED} /> };

export const Sizes: Story = {
  render: () => (
    <div className="flex flex-col gap-sm">
      {(['sm', 'md', 'lg'] as Size[]).map((size) => (
        <Live key={size} items={COUNTED} size={size} />
      ))}
    </div>
  ),
};
