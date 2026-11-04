import type { Meta, StoryObj } from '@storybook/react-vite';
import { Activity, AlertTriangle, Cpu, FolderGit2, Gauge, History, LayoutDashboard, Settings } from 'lucide-react';
import type { PeerRowModel } from '../popover/peers';
import { DashboardNav, type DashboardNavItem } from './DashboardNav';
import { HostCard } from './HostCard';
import { UpdateCard } from './UpdateCard';

// DashboardNav's @states. The labels below are example data only: the IA is decided at the screen
// step (anatomy.md § 2 O1), so one story also draws ADR-0041's groups in `section` mode.

const peer: PeerRowModel = {
  key: 'a1',
  name: 'peer beta',
  runners: 2,
  pips: [
    { kind: 'running', key: 'a1-0' },
    { kind: 'running', key: 'a1-1' },
  ],
  stale: false,
  age: '12s',
  label: 'peer beta, 2 runners, seen 12 seconds ago',
};

const host = <HostCard name="alpha" tone="green" summary="2 hosts, both healthy" peers={[peer]} />;
const footer = (
  <UpdateCard version="0.3.5" body="Installs when this host goes idle." onInstall={() => {}} onSkip={() => {}} />
);

const items: DashboardNavItem[] = [
  { id: 'overview', label: 'Overview', icon: LayoutDashboard, href: '/' },
  { id: 'activity', label: 'Activity', icon: Activity, href: '/activity' },
  { id: 'memory', label: 'Memory', icon: Gauge, href: '/memory' },
  { id: 'repositories', label: 'Repositories', icon: FolderGit2, href: '/repositories' },
  { id: 'settings', label: 'Settings', icon: Settings, href: '/settings' },
];

const withCounts: DashboardNavItem[] = items.map((i) =>
  i.id === 'activity'
    ? { ...i, count: 7, countTone: 'amber', countLabel: '7 faults' }
    : i.id === 'repositories'
      ? { ...i, count: 9, countLabel: '9 repositories' }
      : i.id === 'memory'
        ? { ...i, count: '67%', countTone: 'amber', countLabel: '67 percent used' }
        : i,
);

const adr0041: DashboardNavItem[] = [
  { id: 'attention', label: 'Needs attention', icon: AlertTriangle, href: '#attention', count: 2, countTone: 'amber', countLabel: '2 items' },
  { id: 'now', label: 'Now', icon: Activity, href: '#now' },
  { id: 'capacity', label: 'Capacity', icon: Cpu, href: '#capacity' },
  { id: 'history', label: 'History', icon: History, href: '#history' },
  { id: 'settings', label: 'Settings', icon: Settings, href: '#settings' },
];

const meta = {
  title: 'Blocks/DashboardNav',
  component: DashboardNav,
  args: { items, selected: 'overview', mode: 'page', version: '0.3.4', host, footer },
  render: (args) => (
    <div className="flex h-dvh font-sans text-body text-fg">
      <DashboardNav {...args} />
    </div>
  ),
} satisfies Meta<typeof DashboardNav>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'a[href="/memory"]' } } };
export const Selected: Story = { args: { selected: 'activity' } };
export const Focused: Story = { parameters: { force: { 'focus-visible': 'a[href="/memory"]' } } };
export const WithCounts: Story = { args: { items: withCounts, selected: 'activity' } };
export const WithFooter: Story = { args: { items: withCounts } };
export const NoFooter: Story = { args: { footer: undefined } };

/** ADR-0041's four groups plus Settings, as jump links on one page: `aria-current="location"`. */
export const SectionMode: Story = { args: { items: adr0041, selected: 'now', mode: 'section' } };

/** A count that is a word ("Paused") sets in the sans face; the numbers stay mono. */
export const WithWordCount: Story = {
  args: {
    items: withCounts.map((i) => (i.id === 'settings' ? { ...i, count: 'Paused', countKind: 'word', countTone: 'amber', countLabel: 'the host is paused' } : i)),
    selected: 'activity',
  },
};
