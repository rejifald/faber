import type { Meta, StoryObj } from '@storybook/react-vite';
import type { MirrorEcosystemView, MirrorView } from '../popover/types';
import { MirrorSection } from './MirrorSection';

// The dashboard's Mirror section, one story per state. Repos are owner/demo style only (AGENTS.md).

const GIB = 1 << 30;
const CAP = 20 * GIB;

const eco = (over: Partial<MirrorEcosystemView> = {}): MirrorEcosystemView => ({
  fill: 'relaxed',
  objects: 0,
  bytes: 0,
  max_bytes: CAP,
  repos: [],
  ...over,
});

const view = (over: Partial<MirrorEcosystemView> = {}): MirrorView => ({
  state: 'ok',
  max_size_gib: 20,
  attention: [],
  lfs: eco(over),
});

const repos = [
  {
    repo: 'owner/demo',
    hits: 1800,
    misses: 200,
    bytes_from_cache: 40 * GIB,
    bytes_from_upstream: 5 * GIB,
    passed_through: 0,
  },
  {
    repo: 'owner/site',
    hits: 100,
    misses: 30,
    bytes_from_cache: 0.5 * GIB,
    bytes_from_upstream: 0.25 * GIB,
    passed_through: 3,
  },
];

// npm counts requests (packuments and tarballs), so its counts run higher and its bytes lower. A
// request on an address no sync had named yet is counted as (unattributed).
const npmRepos = [
  {
    repo: 'owner/demo',
    hits: 15000,
    misses: 1400,
    bytes_from_cache: 3 * GIB,
    bytes_from_upstream: 0.5 * GIB,
    passed_through: 10,
  },
  {
    repo: '(unattributed)',
    hits: 0,
    misses: 4,
    bytes_from_cache: 0,
    bytes_from_upstream: 800 * 1024,
    passed_through: 0,
  },
];

const meta = {
  title: 'Blocks/MirrorSection',
  component: MirrorSection,
  args: { mirror: view() },
  render: (args) => (
    <div className="bg-bg p-3xl font-sans">
      <MirrorSection {...args} />
    </div>
  ),
} satisfies Meta<typeof MirrorSection>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Turned on, nothing fetched through it yet. */
export const Empty: Story = {};

/** Two repos fetching through it, the store well under its cap. */
export const Populated: Story = { args: { mirror: view({ objects: 2300, bytes: 5 * GIB, repos }) } };

/** The store at 95% of its cap: eviction makes room, which the section says quietly. */
export const CapNearlyFull: Story = { args: { mirror: view({ objects: 9000, bytes: 19 * GIB, repos }) } };

/** `fill = "off"`: hits still served, misses passed through, said once, in words. */
export const FillOff: Story = { args: { mirror: view({ fill: 'off', objects: 2300, bytes: 5 * GIB, repos }) } };

/** `[mirror.npm]` alone, turned on, nothing installed through it yet. */
export const NpmEmpty: Story = {
  args: { mirror: { state: 'ok', max_size_gib: 20, attention: [], npm: eco() } },
};

/** `[mirror.npm]` alone: tarballs in the store, requests per repo, one of them unattributed. */
export const NpmOnly: Story = {
  args: {
    mirror: {
      state: 'ok',
      max_size_gib: 20,
      attention: [],
      npm: eco({ objects: 4200, bytes: 2 * GIB, repos: npmRepos }),
    },
  },
};

/** Both ecosystems under the one cap, npm's fill off. */
export const LfsAndNpm: Story = {
  args: {
    mirror: {
      ...view({ objects: 2300, bytes: 5 * GIB, repos }),
      npm: eco({ fill: 'off', objects: 4200, bytes: 2 * GIB, repos: npmRepos }),
    },
  },
};
