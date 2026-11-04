import type { Meta, StoryObj } from '@storybook/react-vite';
import { navItems } from '../settings/nav';
import { settingsView } from '../settings/fixtures';
import { SettingsNav } from './SettingsNav';

// SettingsNav, with its search, as docs/design/settings/anatomy.md describes it.

const items = navItems(settingsView.schema, 3, true);

const meta = {
  title: 'Blocks/SettingsNav',
  component: SettingsNav,
  args: { items, selected: 'autolimiter', onSelect: () => {}, query: '', onQuery: () => {} },
  render: (args) => (
    <div className="flex font-sans text-body text-fg">
      <SettingsNav {...args} />
    </div>
  ),
} satisfies Meta<typeof SettingsNav>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const SelectedProfiles: Story = { args: { selected: 'profiles' } };
export const Hover: Story = { parameters: { force: { hover: 'a[href="#trust"]' } } };
export const Focused: Story = { parameters: { force: { 'focus-visible': 'a[href="#trust"]' } } };
export const SearchHits: Story = {
  args: {
    query: 'disk',
    results: [
      { id: 'docker-host', section: 'Docker host', field: 'Disk floor' },
      { id: 'docker-host', section: 'Docker host', field: 'Cache max age' },
    ],
  },
};
export const NoHits: Story = { args: { query: 'zzz', results: [] } };
