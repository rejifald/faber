import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { ToggleRow } from './ToggleRow';

// One story per state in ToggleRow's @states. Hover and focus are forced on the first row.

const count = 'ml-auto font-mono text-caption font-normal text-faint group-data-pressed/toggle:text-accent-text';

function Live({ on = false, weight }: { on?: boolean; weight?: 'regular' | 'semibold' }) {
  const [pressed, setPressed] = useState(on);
  return (
    <div className="w-rail">
      <ToggleRow pressed={pressed} onPressedChange={setPressed} weight={weight}>
        Delivered
        <span className={count}>7</span>
      </ToggleRow>
    </div>
  );
}

const meta = {
  title: 'Kit/ToggleRow',
  component: ToggleRow,
  args: { pressed: false, onPressedChange: () => {}, children: 'Delivered' },
  render: () => <Live />,
} satisfies Meta<typeof ToggleRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Off: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'button' } } };
export const On: Story = { render: () => <Live on /> };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'button' } } };

/** The label in regular weight, for a long list of values. */
export const RegularWeight: Story = { render: () => <Live weight="regular" /> };
