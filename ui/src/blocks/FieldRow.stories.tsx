import type { Meta, StoryObj } from '@storybook/react-vite';
import { fields, profileFields, profiles } from '../settings/fixtures';
import { FieldRow } from './FieldRow';

// FieldRow, one story per kind and per state (settings/anatomy.md § 5).

const meta = {
  title: 'Blocks/FieldRow',
  component: FieldRow,
  args: { field: fields.reserve, value: 2048, loaded: 2048, onChange: () => {} },
  render: (args) => (
    <div className="bg-surface px-xl font-sans text-body text-fg">
      <FieldRow {...args} />
    </div>
  ),
} satisfies Meta<typeof FieldRow>;
export default meta;
type Story = StoryObj<typeof meta>;

const pf = (key: string) => profileFields.find((p) => p.key === key)!;
const env = profiles[0].env as Record<string, string>;

export const KindInt: Story = {};
export const KindFloat: Story = { args: { field: fields.margin, value: 1.2, loaded: 1.2 } };
export const KindText: Story = { args: { field: fields.topic, value: 'faber', loaded: 'faber' } };
export const KindBool: Story = { args: { field: fields.reviveDocker, value: true, loaded: true } };
export const KindBoolOptional: Story = { args: { field: fields.pullUnqualified, value: null, loaded: null } };
export const KindEnum: Story = { args: { field: fields.provisioning, value: 'demand', loaded: 'demand' } };
export const KindEnumOptional: Story = { args: { field: fields.cpu, value: 'auto', loaded: 'auto' } };
export const KindCeilingAuto: Story = { args: { field: fields.ceiling, value: 'auto', loaded: 'auto' } };
export const KindCeilingNumber: Story = { args: { field: fields.ceiling, value: 4, loaded: 4 } };
export const KindLines: Story = { args: { field: fields.priority, value: ['owner/demo', 'owner/docs'], loaded: ['owner/demo', 'owner/docs'] } };
export const KindCsv: Story = { args: { field: pf('drop_grants'), value: ['192.0.2.5/32 5432/tcp'], loaded: ['192.0.2.5/32 5432/tcp'], profile: { name: 'linux-docker' } } };
export const KindMap: Story = { args: { field: pf('env'), value: env, loaded: env, profile: { name: 'linux-docker' } } };
export const KindSudoReadOnly: Story = { args: { field: pf('labels'), value: ['faber', 'docker'], loaded: ['faber', 'docker'], profile: { name: 'linux-docker' }, tagged: false } };
export const KindSudoNarrow: Story = { args: { field: fields.publicRepos, value: true, loaded: true, tagged: false } };

export const StateLive: Story = { args: { field: fields.reserve } };
export const StateRestart: Story = { args: { field: fields.overcommit, value: 2, loaded: 2 } };
export const StateChanged: Story = { args: { field: fields.headroom, value: 1536, loaded: 1024 } };
export const StateChangedNarrowing: Story = { args: { field: pf('dind'), value: false, loaded: true, profile: { name: 'linux-docker' }, tagged: false } };
export const StateInvalid: Story = { args: { field: fields.headroom, value: 1536.5, loaded: 1536, error: 'Headroom must be a whole number of MiB.' } };
/** The two-column layout the list fields use, with the label column at its width. */
export const LayoutStacked: Story = { args: { field: fields.reserve, layout: 'stacked' } };
