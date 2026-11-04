import type { Meta, StoryObj } from '@storybook/react-vite';
import { profileFields } from '../settings/fixtures';
import { fields } from '../settings/fixtures';
import { TrustRow } from './TrustRow';

// TrustRow's @states as Trust.png draws them, and as docs/design/settings/anatomy.md describes them in a profile.

const field = (key: string) => profileFields.find((p) => p.key === key)!;

const meta = {
  title: 'Blocks/TrustRow',
  component: TrustRow,
  args: { field: field('labels'), value: ['faber', 'docker'], profile: { name: 'linux-docker' }, onNarrow: () => {}, onUndo: () => {} },
  render: (args) => (
    <div className="w-popover bg-surface p-xl font-sans text-body text-fg">
      <TrustRow {...args} />
    </div>
  ),
} satisfies Meta<typeof TrustRow>;
export default meta;
type Story = StoryObj<typeof meta>;

/** No narrowing applies, so the command that changes it shows, to copy. */
export const AtRest: Story = {};
export const NarrowingOffered: Story = { args: { field: fields.publicRepos, value: true, profile: undefined } };
export const NarrowingPending: Story = { args: { field: field('dind'), value: true, pending: false } };
/** A long command wraps onto as many lines as it needs, never cut. */
export const LongCommandWraps: Story = { args: { field: field('image'), value: 'ghcr.io/owner/demo-runner:2.329.0', profile: { name: 'a-profile-with-a-rather-long-name-for-wrapping' } } };
export const Empty: Story = { args: { field: field('drop_grants'), value: [] } };
/** A held section's field: its value, and no command, since validation refuses the section on. */
export const Held: Story = { args: { field: fields.registryServer, value: 'ghcr.io', profile: undefined } };
/** A held switch that is on: turning it off is still offered. */
export const HeldNarrowingOffered: Story = { args: { field: fields.registryEnabled, value: true, profile: undefined } };
