import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pause } from 'lucide-react';
import type { ForcedState } from '../../.storybook/force';
import { click } from '../../.storybook/play';
import { SplitButton } from './SplitButton';

// One story per state in SplitButton's @states. Hover, pressed and keyboard focus land on one
// half, the one a pointer or the keyboard would be on, beside a resting control; each half gets
// its own story. The menu opens in a play function.

const meta = {
  title: 'Kit/SplitButton',
  component: SplitButton,
  args: {
    children: 'Pause',
    icon: Pause,
    menuLabel: 'More pause options',
    items: [{ label: 'Drain', onSelect: () => {} }],
  },
} satisfies Meta<typeof SplitButton>;
export default meta;
type Story = StoryObj<typeof meta>;

const MAIN = '[role="group"] > :first-child';
const TRIGGER = '[role="group"] > :last-child';

/** `state` forced on one half of the first control; the second control rests. */
const forced = (state: ForcedState, half: string): Story => ({
  parameters: { force: { [state]: half } },
  render: (args) => (
    <>
      <SplitButton {...args} />
      <SplitButton {...args} />
    </>
  ),
});

export const Default: Story = {};

export const HoverMain = forced('hover', MAIN);
export const HoverTrigger = forced('hover', TRIGGER);

export const PressedMain = forced('pressed', MAIN);
export const PressedTrigger = forced('pressed', TRIGGER);

export const FocusVisibleMain = forced('focus-visible', MAIN);
export const FocusVisibleTrigger = forced('focus-visible', TRIGGER);

/** The ▾ opened with the pointer: the less common sibling, Drain. */
export const MenuOpen: Story = {
  play: (context) => click(context, '[aria-label="More pause options"]', '[role="menu"]'),
};

/** The main half shows the spinner and ignores presses; the ▾ half stays live. */
/** The main half shows a spinner; the ▾ half is disabled until it ends. */
export const Loading: Story = { args: { loading: true } };
