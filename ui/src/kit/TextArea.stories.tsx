import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { TextArea } from './TextArea';

// One story per state in TextArea's @states.

const SAMPLE = '# faber\ntopic = "faber"\n\n[autolimit]\nreserve_mib = 2048\nheadroom_mib = 1536\n';

const meta = { title: 'Kit/TextArea', component: TextArea, args: { label: 'config.toml', value: SAMPLE, onValueChange: () => {} } } satisfies Meta<typeof TextArea>;
export default meta;
type Story = StoryObj<typeof meta>;

function Editable(args: React.ComponentProps<typeof TextArea>) {
  const [value, set] = useState(args.value);
  return (
    <div className="w-popover">
      <TextArea {...args} value={value} onValueChange={set} />
    </div>
  );
}

export const Default: Story = { render: (args) => <Editable {...args} /> };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'textarea' } }, render: (args) => <Editable {...args} /> };
export const Invalid: Story = { args: { invalid: true }, render: (args) => <Editable {...args} /> };
