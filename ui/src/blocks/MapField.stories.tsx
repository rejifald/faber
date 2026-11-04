import type { Meta, StoryObj } from '@storybook/react-vite';
import { profileFields, profiles } from '../settings/fixtures';
import { MapField } from './MapField';

// MapField's @states as docs/design/settings/anatomy.md describes them.

const env = profiles[0].env as Record<string, string>;

const meta = {
  title: 'Blocks/MapField',
  component: MapField,
  args: { field: profileFields.find((p) => p.key === 'env')!, entries: env, loaded: env, profile: { name: 'linux-docker' }, onChange: () => {} },
  render: (args) => (
    <div className="bg-surface p-xl font-sans text-body text-fg">
      <MapField {...args} />
    </div>
  ),
} satisfies Meta<typeof MapField>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const EntryRemoved: Story = { args: { entries: { HTTPS_PROXY: env.HTTPS_PROXY, NO_PROXY: env.NO_PROXY } } };
export const EveryEntryRemoved: Story = { args: { entries: {} } };
export const Empty: Story = { args: { entries: {}, loaded: {}, empty: "No variables. faber passes none of its own to this profile's runners." } };
