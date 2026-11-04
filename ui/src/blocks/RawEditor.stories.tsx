import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { RawEditor } from './RawEditor';

// RawEditor's @states as Raw.png draws them: clean, changed lines tinted, a TOML error.

const LOADED = ['# faber: watched repos carry the topic below.', 'topic = "faber"', 'sweep_interval_secs = 600', '', '[autolimit]', 'reserve_mib = 2048', 'headroom_mib = 1536', 'margin = 1.2', ''].join('\n');
const PATH = '/Library/Application Support/faber/config.toml';

const meta = { title: 'Blocks/RawEditor', component: RawEditor, args: { text: LOADED, loaded: LOADED, path: PATH, onChange: () => {} } } satisfies Meta<typeof RawEditor>;
export default meta;
type Story = StoryObj<typeof meta>;

function Live(args: React.ComponentProps<typeof RawEditor>) {
  const [text, set] = useState(args.text);
  return (
    <div className="w-popover font-sans">
      <RawEditor {...args} text={text} onChange={set} />
    </div>
  );
}

export const Clean: Story = { render: (args) => <Live {...args} /> };
export const Changed: Story = { args: { text: LOADED.replace('600', '900').replace('1536', '1024') }, render: (args) => <Live {...args} /> };
export const TomlError: Story = {
  args: { text: LOADED.replace('1536', '1536 MiB'), error: { line: 7, column: 15, message: 'expected a number after headroom_mib =' } },
  render: (args) => <Live {...args} />,
};
