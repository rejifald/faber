import type { Meta, StoryObj } from '@storybook/react-vite';
import { Button } from '../kit';
import { Notice } from './Notice';

// Notice's tones and parts, as docs/design/settings/anatomy.md describes them.

const meta = { title: 'Blocks/Notice', component: Notice, args: { tone: 'amber', title: 'Restart to apply' } } satisfies Meta<typeof Notice>;
export default meta;
type Story = StoryObj<typeof meta>;

const wrap = (node: React.ReactNode) => <div className="w-popover font-sans">{node}</div>;

export const Amber: Story = {
  args: { body: 'Sweep interval changes when faber restarts.' },
  render: (args) => wrap(<Notice {...args} />),
};
export const RedWithCommand: Story = {
  args: {
    tone: 'red',
    title: 'Not saved: this widens trust',
    body: 'Removing linux moves its jobs onto linux-docker, which has Docker access. A change like this needs root.',
    command: 'sudo faber config apply config.toml',
    actions: (
      <>
        <Button>Discard</Button>
        <Button>Copy command</Button>
      </>
    ),
  },
  render: (args) => wrap(<Notice {...args} />),
};
export const AmberWithActions: Story = {
  args: {
    title: 'Form changes not saved',
    body: 'You changed 2 settings in the form. Save or discard them before editing the file as text.',
    actions: (
      <>
        <Button>Discard</Button>
        <Button variant="primary">Save</Button>
      </>
    ),
  },
  render: (args) => wrap(<Notice {...args} />),
};
