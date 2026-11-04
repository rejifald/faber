import type { Meta, StoryObj } from '@storybook/react-vite';
import type { ForcedState } from '../../.storybook/force';
import { TextLink } from './TextLink';

// One story per state in TextLink's @states. Hover and keyboard focus land on one link, beside a
// resting one.

const meta = { title: 'Kit/TextLink', component: TextLink, args: { href: '/activity', children: 'All activity' } } satisfies Meta<typeof TextLink>;
export default meta;
type Story = StoryObj<typeof meta>;

const forced = (state: ForcedState): Story => ({
  parameters: { force: { [state]: 'a' } },
  render: () => (
    <>
      <TextLink href="/activity">All activity</TextLink>
      <TextLink href="/memory">Open Memory</TextLink>
    </>
  ),
});

export const Default: Story = {};
export const Hover: Story = forced('hover');
export const KeyboardFocus: Story = forced('focus-visible');
export const WithArrow: Story = { args: { arrow: true } };
