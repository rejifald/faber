import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { Switch } from './Switch';
import type { Size } from './ladder';

// One story per state in Switch's @states, plus the ladder steps. Hover, pressed and focus land
// on the first of two, beside a resting one.

const meta = {
  title: 'Kit/Switch',
  component: Switch,
  args: { label: 'Git mirror', checked: false, onCheckedChange: () => {} },
} satisfies Meta<typeof Switch>;
export default meta;
type Story = StoryObj<typeof meta>;

const SIZES: Size[] = ['sm', 'md', 'lg'];

const pair = (state: 'hover' | 'pressed' | 'focus-visible'): Story => ({
  parameters: { force: { [state]: '[role="switch"]' } },
  render: (args) => (
    <div className="flex items-center gap-sm">
      <Switch {...args} checked />
      <Switch {...args} label="Pull-through mirror" />
    </div>
  ),
});

export const Off: Story = {};

export const On: Story = { args: { checked: true } };

export const Hover = pair('hover');

export const Focused = pair('focus-visible');

export const Pressed = pair('pressed');

/** Disabled only while a sibling action in the same place runs, such as the settings saving. */
export const Disabled: Story = {
  render: (args) => (
    <div className="flex items-center gap-sm">
      <Switch {...args} disabled />
      <Switch {...args} checked disabled />
    </div>
  ),
};

function Live({ size }: { size: Size }) {
  const [on, setOn] = useState(true);
  return <Switch label={`Git mirror (${size})`} size={size} checked={on} onCheckedChange={setOn} />;
}

export const Sizes: Story = {
  render: () => (
    <div className="flex items-center gap-sm">
      {SIZES.map((size) => (
        <Live key={size} size={size} />
      ))}
    </div>
  ),
};
