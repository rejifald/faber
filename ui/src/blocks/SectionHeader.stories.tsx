import type { Meta, StoryObj } from '@storybook/react-vite';
import { Button } from '../kit';
import { SectionHeader } from './SectionHeader';

// SectionHeader as docs/design/settings/anatomy.md describes it.

const meta = {
  title: 'Blocks/SectionHeader',
  component: SectionHeader,
  args: { title: 'Autolimiter', hint: 'How much of the engine faber admits jobs into. Changes apply to the running daemon at once.' },
  render: (args) => (
    <div className="bg-surface p-xl font-sans text-body text-fg">
      <SectionHeader {...args} />
    </div>
  ),
} satisfies Meta<typeof SectionHeader>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const WithAction: Story = {
  args: { title: 'Profiles', hint: 'What a job runs in. A job goes to the first profile whose labels its runs-on matches, so the order matters. Applies on restart.', action: <Button>Add profile</Button> },
};
export const NoHint: Story = { args: { hint: undefined } };
