import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pipe } from './Pipe';
import { StatTile, type StatTileProps } from './StatTile';

// StatTile as the Overview draws its four headline figures (dashboard/anatomy.md § 5,
// reference/B-Overview.png).

const meta = { title: 'Blocks/StatTile', component: StatTile } satisfies Meta<typeof StatTile>;
export default meta;
type Story = StoryObj<typeof meta>;

const unit = (text: string) => <span className="text-small font-normal text-muted">{text}</span>;

const delivered: StatTileProps = {
  label: 'Delivered',
  value: (
    <>
      95.0
      {unit('%')}
    </>
  ),
  note: '20 of 400 did not deliver',
  trend: { values: [90, 91, 91, 92, 93, 93, 95], tone: 'green', label: 'Delivered rate over 24 hours, rising' },
};

const faults: StatTileProps = {
  label: "faber's own faults",
  value: '7',
  tone: 'amber',
  note: '2 in the last hour',
  trend: { values: [1, 3, 1, 4, 2, 6, 2, 5, 3], tone: 'amber', label: 'Faults over 24 hours, spiky' },
};

const wait: StatTileProps = {
  label: 'Wait to admit',
  value: (
    <>
      4{unit('s')}
    </>
  ),
  note: (
    <>
      p95 1m 40s
      <Pipe />
      budget is tight
    </>
  ),
  trend: { values: [3, 4, 3, 5, 4, 4, 5, 4], label: 'Wait to admit over 24 hours, steady' },
};

const refused: StatTileProps = {
  label: 'Refused at the gate',
  value: '23',
  note: '21 untrusted actor, 2 public repo',
  trend: { values: [5, 6, 5, 6, 5, 6, 5, 6], label: 'Refusals over 24 hours, steady' },
};

export const Default: Story = { args: delivered };
export const Toned: Story = { args: faults };
export const WithTrend: Story = { args: wait };
export const NoTrend: Story = { args: { label: refused.label, value: refused.value, note: refused.note } };
export const Loading: Story = { args: { label: 'Delivered', value: '—' } };

/** The four tiles of the Overview, in one row. */
export const Overview: Story = {
  args: { label: '', value: '' },
  render: () => (
    <div className="flex gap-lg *:flex-1">
      <StatTile {...delivered} />
      <StatTile {...faults} />
      <StatTile {...wait} />
      <StatTile {...refused} />
    </div>
  ),
};

/** The class detail's three boxes: title-size value, a note of up to two lines. */
export const Small: Story = {
  args: { label: 'Cap in force', value: <>6.4{unit(' GiB')}</>, note: 'Learned from 47 samples; pinned override not set', size: 'sm' },
  render: (args) => (
    <div className="flex gap-lg *:flex-1">
      <StatTile {...args} />
      <StatTile label="Measured peak" value={<>5.9{unit(' GiB')}</>} note="Highest of the last 20 runs" size="sm" />
      <StatTile label="Pinned override" value="—" note="None set" size="sm" />
    </div>
  ),
};
