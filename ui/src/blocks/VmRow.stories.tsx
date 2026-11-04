import type { Meta, StoryObj } from '@storybook/react-vite';
import { NOW } from '../popover/fixtures';
import type { VmEntry } from '../popover/types';
import { StoryCard } from '../popover/StoryCard';
import { vmSection } from '../popover/vms';
import { VmRow } from './VmRow';

// VmRow's @states, from the model the screen builds (popover/vms.ts), in the 240 px left column.

const up = (s: number) => Math.floor(NOW / 1000) - s;
const row = (vm: Partial<VmEntry>, docker_inflight = 0, listed_age_s = 5) => {
  const section = vmSection(
    {
      docker_inflight,
      disk: { vm_free_mib: 40 * 1024 },
      vm: { listed_age_s, vms: [{ kind: 'macos', slot: 0, state: 'in_service', started_unix: up(720), cpus: 4, memory_mib: 8192, job: null, ...vm }] },
    },
    NOW,
  );
  return { vm: section.rows[0], dim: section.dim, open: false, onOpenChange: () => {} };
};

const linux = { kind: 'linux', cpus: 6, memory_mib: 16 * 1024, started_unix: up(7200) };

const meta = {
  title: 'Blocks/VmRow',
  component: VmRow,
  args: row({ ...linux }, 3),
  render: (args) => (
    <StoryCard inset={false}>
      <div className="w-gauge-col">
        <VmRow {...args} />
      </div>
    </StoryCard>
  ),
} satisfies Meta<typeof VmRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const ServingLinux: Story = {};
export const ServingMacos: Story = { args: row({ job: { repo: 'owner/demo', job_name: 'release' } }) };
export const Idle: Story = { args: row({ ...linux }, 0) };
/** Open: what it was given and its free disk, as a gate lists its parts. */
export const Open: Story = { args: { ...row({ ...linux }, 3), open: true } };
export const Booting: Story = { args: row({ state: 'booting', started_unix: up(40) }) };
export const Stopping: Story = { args: row({ state: 'stopping' }) };
export const Unmanaged: Story = { args: row({ state: 'unmanaged' }) };
/** A VM adopted after a restart whose job is unknown: no size, so nothing to open. */
export const NoSpec: Story = { args: row({ cpus: null, memory_mib: null }) };
export const LongJobName: Story = {
  args: row({ job: { repo: 'owner/demo', job_name: 'release / build, test, package and publish every artifact for the next version' } }),
};
/** The supervisor is unreachable: a neutral icon, the age `—`. */
export const Dim: Story = { args: row({ job: { repo: 'owner/demo', job_name: 'release' } }, 0, 180) };
