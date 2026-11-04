import type { Meta, StoryObj } from '@storybook/react-vite';
import { Download, Ellipsis, ExternalLink, LogOut } from 'lucide-react';
import { click, keys } from '../../.storybook/play';
import type { Size } from './ladder';
import { useState } from 'react';
import { Menu, MenuCheckboxItem, MenuItem, MenuLabel, MenuSeparator } from './Menu';

// Menu's @states (closed, open, item highlighted, item disabled), MenuItem's (default,
// highlighted, disabled) and MenuSeparator's, plus the trigger's ladder steps. A popup opens in a
// play function: a click, then the keyboard.

const meta = {
  title: 'Kit/Menu',
  component: Menu,
  args: { label: 'More actions', icon: Ellipsis, iconOnly: true, children: null },
  render: (args) => (
    <Menu {...args}>
      <MenuItem icon={ExternalLink}>Open dashboard</MenuItem>
      <MenuItem icon={Download} disabled>
        Check for updates
      </MenuItem>
      <MenuSeparator />
      <MenuItem icon={LogOut} tone="danger">
        Quit
      </MenuItem>
    </Menu>
  ),
} satisfies Meta<typeof Menu>;
export default meta;
type Story = StoryObj<typeof meta>;

const SIZES: Size[] = ['sm', 'md', 'lg'];

const open = (context: { canvasElement: HTMLElement }) => click(context, 'button', '[role="menu"]');

export const Closed: Story = {};

export const ClosedWithLabel: Story = { args: { iconOnly: false, label: 'Actions' } };

/** Open with the pointer: nothing highlighted, one item disabled, a separator, a danger item. */
export const Open: Story = { play: open };

/** Open, with one item highlighted by the keyboard, as the pointer would: never the whole list. */
export const ItemHighlighted: Story = {
  play: async (context) => {
    await open(context);
    await keys('{ArrowDown}');
  },
};

/**
 * The disabled item is skipped by the keyboard and shows dimmed. An item is disabled only while a
 * sibling action in the same place runs: Check for updates, while an update installs.
 */
export const ItemDisabled: Story = {
  play: async (context) => {
    await open(context);
    await keys('{ArrowDown}{ArrowDown}');
  },
};

export const TriggerSizes: Story = {
  render: () => (
    <>
      {SIZES.map((size) => (
        <Menu key={size} size={size} label={`Actions ${size}`} icon={Ellipsis}>
          <MenuItem>Open dashboard</MenuItem>
        </Menu>
      ))}
      {SIZES.map((size) => (
        <Menu key={`${size}-icon`} size={size} label={`More actions (${size})`} icon={Ellipsis} iconOnly>
          <MenuItem>Open dashboard</MenuItem>
        </Menu>
      ))}
    </>
  ),
};

/** The popover's ⋯ menu: shortcuts, a checkbox item, and an item that stays open to show its new value. */
function SettingsMenu({ checked: initial }: { checked: boolean }) {
  const [checked, setChecked] = useState(initial);
  return (
    <Menu label="More" icon={Ellipsis} iconOnly>
      <MenuLabel>faber v0.3.13</MenuLabel>
      <MenuSeparator />
      <MenuItem shortcut="⌘⇧F">Open panel</MenuItem>
      <MenuCheckboxItem checked={checked} onCheckedChange={setChecked}>
        Start at login
      </MenuCheckboxItem>
      <MenuItem keepOpen>App update wait: 6 h</MenuItem>
      <MenuSeparator />
      <MenuItem shortcut="⌘Q">Quit faber</MenuItem>
    </Menu>
  );
}

/** MenuCheckboxItem unchecked, under a MenuLabel, with shortcuts on the plain items. */
export const CheckboxUnchecked: Story = { render: () => <SettingsMenu checked={false} />, play: open };

/** MenuCheckboxItem checked: the tick at the trailing edge. */
export const CheckboxChecked: Story = { render: () => <SettingsMenu checked />, play: open };

/** MenuCheckboxItem highlighted by the keyboard. */
export const CheckboxHighlighted: Story = {
  render: () => <SettingsMenu checked />,
  play: async (context) => {
    await open(context);
    await keys('{ArrowDown}{ArrowDown}');
  },
};

/** MenuLabel `error`: the note under an item that could not act, announced and in red. */
export const LabelError: Story = {
  render: () => (
    <Menu label="More" icon={Ellipsis} iconOnly>
      <MenuLabel>faber v0.3.13</MenuLabel>
      <MenuSeparator />
      <MenuItem keepOpen>App update wait: 6 h</MenuItem>
      <MenuLabel tone="error">The app could not save the wait: disk full</MenuLabel>
    </Menu>
  ),
  play: open,
};
