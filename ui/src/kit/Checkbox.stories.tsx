import type { Meta, StoryObj } from '@storybook/react-vite';
import { Checkbox } from './Checkbox';
import type { Size } from './ladder';

// One story per state in Checkbox's @states, plus the ladder steps.

const meta = {
  title: 'Kit/Checkbox',
  component: Checkbox,
  args: { label: 'Keep the Mac awake while jobs run' },
} satisfies Meta<typeof Checkbox>;
export default meta;
type Story = StoryObj<typeof meta>;

const SIZES: Size[] = ['sm', 'md', 'lg'];

export const Unchecked: Story = {};

export const Checked: Story = { args: { defaultChecked: true } };

/** Keyboard focus on the first of two. */
export const FocusVisible: Story = {
  parameters: { force: { 'focus-visible': '[role="checkbox"]' } },
  render: (args) => (
    <>
      <Checkbox {...args} />
      <Checkbox {...args} label="Show finished jobs" defaultChecked />
    </>
  ),
};

/** Disabled only while a sibling action in the same place runs, such as the settings saving. */
export const Disabled: Story = {
  render: (args) => (
    <>
      <Checkbox {...args} disabled />
      <Checkbox {...args} label="Show finished jobs" disabled defaultChecked />
    </>
  ),
};

export const Sizes: Story = {
  render: () => (
    <div className="flex flex-col gap-sm">
      {SIZES.map((size) => (
        <Checkbox key={size} size={size} label={`Keep the Mac awake (${size})`} defaultChecked={size !== 'sm'} />
      ))}
    </div>
  ),
};
