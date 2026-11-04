import type { Meta, StoryObj } from '@storybook/react-vite';
import { Search } from 'lucide-react';
import { Field, Input } from './Field';
import { Select } from './Select';

// Field's @states (default, disabled, invalid) and Input's (default, focus-visible, disabled,
// invalid). Input takes its disabled and invalid states from the Field around it.

const meta = {
  title: 'Kit/Field',
  component: Field,
  args: { label: 'Runner image', description: 'A local tag, never pulled.', children: null },
} satisfies Meta<typeof Field>;
export default meta;
type Story = StoryObj<typeof meta>;

const column = 'flex w-full flex-col gap-lg';

export const Default: Story = {
  render: (args) => (
    <div className={column}>
      <Field {...args}>
        <Input mono placeholder="ghcr.io/owner/my-runner" />
      </Field>
      <Field label="Update channel">
        <Select
          defaultValue="release"
          options={[
            { value: 'release', label: 'Release' },
            { value: 'off', label: 'Off' },
          ]}
        />
      </Field>
    </div>
  ),
};

/** Disabled only while a sibling action in the same place runs, such as the settings saving. */
export const Disabled: Story = {
  render: (args) => (
    <Field {...args} disabled>
      <Input mono defaultValue="ghcr.io/owner/my-runner" />
    </Field>
  ),
};

export const Invalid: Story = {
  render: (args) => (
    <Field {...args} error="Not a valid image reference: it has a space.">
      <Input mono defaultValue="ghcr.io/owner/my runner" />
    </Field>
  ),
};

export const InputEmpty: Story = {
  render: () => (
    <Field label="Instance name">
      <Input placeholder="builder" />
    </Field>
  ),
};

export const InputFilled: Story = {
  render: () => (
    <div className={column}>
      <Field label="Instance name">
        <Input defaultValue="builder" />
      </Field>
      <Field label="Runner image" description="Mono, for paths, IDs and commands.">
        <Input mono defaultValue="ghcr.io/owner/my-runner" />
      </Field>
      <Field label="Concurrent jobs">
        <Input type="number" defaultValue="4" />
      </Field>
    </div>
  ),
};

export const InputFocusVisible: Story = {
  parameters: { force: { 'focus-visible': 'input' } },
  render: () => (
    <Field label="Instance name">
      <Input defaultValue="builder" />
    </Field>
  ),
};

/** A glyph at the start of the box: the Settings nav's search. */
export const InputLeadingIcon: Story = {
  render: () => (
    <div className={column}>
      <Input type="search" leadingIcon={Search} placeholder="Search settings" aria-label="Search settings" />
    </div>
  ),
};

/** A number with its floor and step, named where no Field label wraps it. */
export const InputNumberBounds: Story = {
  render: () => <Input type="number" min={1} step={0.05} defaultValue="1.2" aria-label="Margin" />,
};
