import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { gib } from '../popover/limits';
import { Stepper, stepValue, type StepperProps } from './Stepper';

// One story per state in Stepper's @states. Reserve steps by 512 MiB and shows GiB.

const meta = {
  title: 'Kit/Stepper',
  component: Stepper,
  args: { label: 'Reserve', value: 2048, display: '2.0 GiB', step: 512, onChange: () => {} },
} satisfies Meta<typeof Stepper>;
export default meta;
type Story = StoryObj<typeof meta>;

function Reserve(props: Partial<StepperProps> & { start?: number }) {
  const { start = 2048, ...rest } = props;
  const [mib, setMib] = useState(start);
  return (
    <Stepper
      label="Reserve"
      value={mib}
      display={`${gib(mib)} GiB`}
      step={512}
      min={0}
      onChange={(next) => setMib(stepValue(next, 0, 0))}
      {...rest}
    />
  );
}

export const Default: Story = { render: () => <Reserve /> };
/** At the minimum the decrement stays live and the value stays put. */
export const AtMin: Story = { render: () => <Reserve start={0} /> };
export const Busy: Story = { render: () => <Reserve start={2560} busy /> };
export const Success: Story = { render: () => <Reserve tone="success" /> };
export const Warning: Story = { render: () => <Reserve tone="warning" /> };
export const Focused: Story = { parameters: { force: { 'focus-visible': '[role="spinbutton"]' } }, render: () => <Reserve /> };
export const HoverDecrement: Story = { parameters: { force: { hover: 'button' } }, render: () => <Reserve /> };
export const PressedIncrement: Story = { parameters: { force: { pressed: 'button:last-of-type' } }, render: () => <Reserve /> };
export const Sizes: Story = {
  render: () => (
    <>
      <Reserve size="sm" />
      <Reserve size="md" />
      <Reserve size="lg" />
    </>
  ),
};
