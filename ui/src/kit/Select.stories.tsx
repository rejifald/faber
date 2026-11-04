import type { Meta, StoryObj } from '@storybook/react-vite';
import { click, keys } from '../../.storybook/play';
import { Field } from './Field';
import { Select } from './Select';

// Select's @states: closed, open, item highlighted, item selected, disabled, invalid. It sits in a
// Field, as every Select does, and takes disabled and invalid from it.

const OPTIONS = [
  { value: 'release', label: 'Release' },
  { value: 'prerelease', label: 'Pre-release' },
  { value: 'off', label: 'Off' },
];

const meta = {
  title: 'Kit/Select',
  component: Select,
  args: { options: OPTIONS, placeholder: 'Choose a channel' },
  render: (args) => (
    <Field label="Update channel">
      <Select {...args} />
    </Field>
  ),
} satisfies Meta<typeof Select>;
export default meta;
type Story = StoryObj<typeof meta>;

const open = (context: { canvasElement: HTMLElement }) => click(context, '[role="combobox"]', '[role="listbox"]');

/** Nothing chosen yet: the placeholder. */
export const Closed: Story = {};

export const ClosedWithValue: Story = { args: { defaultValue: 'release' } };

export const Open: Story = { play: open };

/** One item highlighted, as by the pointer or the arrow keys: never the whole list. */
export const ItemHighlighted: Story = {
  play: async (context) => {
    await open(context);
    await keys('{ArrowDown}');
  },
};

/** Open on a chosen value: its check mark. */
export const ItemSelected: Story = { args: { defaultValue: 'prerelease' }, play: open };

/** Disabled only while a sibling action in the same place runs, such as the settings saving. */
export const Disabled: Story = {
  args: { defaultValue: 'release' },
  render: (args) => (
    <Field label="Update channel" disabled>
      <Select {...args} />
    </Field>
  ),
};

export const Invalid: Story = {
  render: (args) => (
    <Field label="Update channel" error="Choose a channel before saving.">
      <Select {...args} />
    </Field>
  ),
};
