import type { Meta, StoryObj } from '@storybook/react-vite';
import { StatusBadge } from '../kit';
import { KeyValueList } from './KeyValueList';

// KeyValueList's @states (default, with dots, toned value, long label, grid): the host health rows
// and the learned caps list from the Overview.

const meta = {
  title: 'Blocks/KeyValueList',
  component: KeyValueList,
  args: {
    items: [
      { key: 'docker', label: 'Docker', value: 'reachable' },
      { key: 'disk', label: 'Disk', value: '90% used' },
      { key: 'pressure', label: 'Memory pressure', value: 'none' },
      { key: 'uptime', label: 'Uptime', value: '6d 4h' },
    ],
  },
  decorators: [(Story) => <div className="w-rail rounded-lg border border-border bg-surface p-lg"><Story /></div>],
} satisfies Meta<typeof KeyValueList>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = { args: { mono: 'value' } };

export const WithDots: Story = {
  args: {
    mono: 'value',
    items: [
      { key: 'docker', label: 'Docker', value: 'reachable', tone: 'green' },
      { key: 'disk', label: 'Disk', value: '90% used', tone: 'amber' },
      { key: 'pressure', label: 'Memory pressure', value: 'none', tone: 'green' },
      { key: 'uptime', label: 'Uptime', value: '6d 4h', tone: 'neutral' },
      // No tone: the row still reserves the dot's column, so its label lines up with the rest.
      { key: 'audit', label: 'Audit log', value: '/var/log/faber/audit.jsonl' },
    ],
  },
};

/** Amber and red tone the value as well as the dot; the learned caps use a badge for status. */
export const TonedValue: Story = {
  args: {
    mono: 'label',
    items: [
      { key: 'web', label: 'acme/web:build', value: <StatusBadge tone="accent">Pinned</StatusBadge> },
      { key: 'api', label: 'acme/api:test', value: <StatusBadge tone="amber">Warned</StatusBadge>, tone: 'amber' },
      { key: 'mobile', label: 'acme/mobile:build', value: '6.4 GiB', tone: 'red', note: '12 samples' },
      { key: 'docs', label: 'acme/docs:lint', value: '1.1 GiB', tone: 'green' },
    ],
  },
};

export const LongLabel: Story = {
  args: {
    mono: 'both',
    items: [
      { key: 'long', label: 'acme/mobile:build-and-sign-release-candidate-for-every-supported-platform', value: '6.4 GiB' },
      { key: 'short', label: 'acme/web:build', value: '2.2 GiB' },
    ],
  },
};

/** Label over value in columns that wrap: the detail of one opened row. */
export const Grid: Story = {
  args: {
    layout: 'grid',
    items: [
      { key: 'runner', label: 'Runner', value: 'faber-4471820291-1a2b' },
      { key: 'attempt', label: 'Attempt', value: '1' },
      { key: 'profile', label: 'Profile', value: 'docker' },
      { key: 'wait', label: 'Wait', value: '4s' },
      { key: 'duration', label: 'Duration', value: '4m 31s' },
      { key: 'job', label: 'Job ID', value: '4471820291' },
    ],
  },
  decorators: [(Story) => <div className="w-full"><Story /></div>],
};
