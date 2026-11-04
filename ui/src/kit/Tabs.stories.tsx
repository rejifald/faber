import type { Meta, StoryObj } from '@storybook/react-vite';
import { Tabs, type TabItem } from './Tabs';

// Tabs' @states: default, hover, selected, focus-visible. A tab has no disabled state.

const panel = (text: string) => <p className="text-body text-muted">{text}</p>;

const TABS: TabItem[] = [
  { value: 'runners', label: 'Runners', content: panel('No runners.') },
  { value: 'settings', label: 'Settings', content: panel('The settings form.') },
  { value: 'audit', label: 'Audit log', content: panel('Nothing logged yet.') },
];

const meta = { title: 'Kit/Tabs', component: Tabs, args: { label: 'Dashboard sections', tabs: TABS } } satisfies Meta<
  typeof Tabs
>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

/** The pointer over one tab that is not selected. */
export const Hover: Story = { parameters: { force: { hover: '[role="tab"][aria-selected="false"]' } } };

/** A tab other than the first selected. */
export const Selected: Story = { args: { defaultValue: 'settings' } };

/** Keyboard focus on the selected tab, where tabbing into the list lands. */
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': '[role="tab"][aria-selected="true"]' } } };
