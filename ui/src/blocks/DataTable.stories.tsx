import { useState, type ComponentProps } from 'react';
import type { Meta, StoryObj } from '@storybook/react-vite';
import { StatusBadge, StatusDot } from '../kit';
import { Pipe } from './Pipe';
import { DataTable, type DataTableColumn, type DataTableRow } from './DataTable';

// DataTable's @states (default, toned row, barred row, expanded row, closed detail row, empty, long text, selectable, selected row), with the Overview's
// two tables: In flight and Activity.

const repo = (name: string, pr: string) => (
  <>
    {name}
    <Pipe />
    <span className="text-muted">{pr}</span>
  </>
);

const flightColumns: DataTableColumn[] = [
  { key: 'state', header: 'State', width: 'tight' },
  { key: 'repo', header: 'Repo' },
  { key: 'job', header: 'Job' },
  { key: 'step', header: 'Step', width: 'fill' },
  { key: 'mem', header: 'Memory', width: 'tight', align: 'end', mono: true },
  { key: 'elapsed', header: 'Elapsed', width: 'tight', align: 'end', mono: true },
];
const flightRows: DataTableRow[] = [
  { key: 'a', cells: { state: <StatusDot tone="accent" pulse />, repo: repo('acme/web', 'PR 412'), job: 'build', step: 'Run tests', mem: '3.1 GiB', elapsed: '4m 12s' } },
  { key: 'b', cells: { state: <StatusDot tone="accent" pulse />, repo: repo('acme/api', 'PR 88'), job: 'test', step: 'Set up job', mem: '1.4 GiB', elapsed: '0m 38s' } },
  { key: 'c', cells: { state: <StatusDot tone="neutral" />, repo: repo('acme/docs', 'main'), job: 'lint', step: 'Waiting for memory', mem: '0.6 GiB', elapsed: '1m 05s' } },
];
const parked: DataTableRow = {
  key: 'p',
  tone: 'amber',
  cells: { state: <StatusDot tone="amber" />, repo: repo('acme/mobile', 'PR 31'), job: 'build', step: 'Parked: needs 6.4 GiB', mem: '6.4 GiB', elapsed: '9m 50s' },
};

const activityColumns: DataTableColumn[] = [
  { key: 'when', header: 'When', width: 'tight', mono: true },
  { key: 'result', header: 'Result', width: 'tight' },
  { key: 'repo', header: 'Repo' },
  { key: 'job', header: 'Job' },
  { key: 'detail', header: 'Detail', width: 'fill' },
  { key: 'took', header: 'Took', width: 'tight', align: 'end', mono: true },
];
const activityRows: DataTableRow[] = [
  { key: '1', cells: { when: '11:52', result: <StatusBadge tone="green">Delivered</StatusBadge>, repo: 'acme/web', job: 'build', detail: 'faber-4471820291-1a2b', took: '4m 31s' } },
  { key: '2', cells: { when: '11:40', result: <StatusBadge tone="red">Refused</StatusBadge>, repo: 'ext/fork', job: 'test', detail: 'Fork job asked for dind', took: '0m 02s' } },
  {
    key: '3',
    tone: 'amber',
    cells: { when: '11:21', result: <StatusBadge tone="amber">Timed out</StatusBadge>, repo: 'acme/mobile', job: 'build', detail: 'faber-4471820291-3c4d', took: '3h 00s' },
    detailLabel: 'Details for acme/mobile build',
    defaultOpen: true,
    detail: 'The runner stayed idle past its 3 h limit and was removed. No job was assigned to it before then.',
  },
  { key: '4', cells: { when: '11:05', result: <StatusBadge tone="neutral">Retried</StatusBadge>, repo: 'acme/api', job: 'test', detail: 'Runner lost communication', took: '2m 14s' } },
];

function Picking({ first, ...args }: ComponentProps<typeof DataTable> & { first: string }) {
  const [key, setKey] = useState(first);
  return <DataTable {...args} selectedKey={key} onSelect={setKey} />;
}

const meta = {
  title: 'Blocks/DataTable',
  parameters: { layout: 'padded' },
  component: DataTable,
  args: { caption: 'Jobs in flight', empty: 'Nothing is running.', columns: flightColumns, rows: flightRows },
  decorators: [(Story) => <div className="w-full rounded-lg border border-border bg-surface py-lg"><Story /></div>],
} satisfies Meta<typeof DataTable>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};
export const TonedRow: Story = { args: { rows: [...flightRows, parked] } };
/** A lead bar flags a row without washing it: amber for a caution, red for a fault. */
export const BarredRow: Story = {
  args: { rows: [flightRows[0], { ...flightRows[1], bar: 'amber' }, { ...parked, bar: 'amber' }, { ...flightRows[2], bar: 'red' }] },
};
export const ExpandedRow: Story = { args: { caption: 'Recent activity', columns: activityColumns, rows: activityRows } };
export const ClosedDetailRow: Story = {
  args: {
    caption: 'Recent activity',
    columns: activityColumns,
    rows: activityRows.map((r) => (r.key === '3' ? { ...r, defaultOpen: false } : r)),
  },
};
export const Empty: Story = { args: { rows: [] } };
export const LongText: Story = {
  args: {
    caption: 'Recent activity',
    columns: activityColumns,
    rows: [
      {
        key: 'long',
        cells: {
          when: '11:52',
          result: <StatusBadge tone="red">Infra error</StatusBadge>,
          repo: 'acme/mobile-app-with-a-very-long-repository-name',
          job: 'build-and-sign-release-candidate',
          detail: 'Runner faber-4471820291-1a2b lost communication with the server while the job was running its test step',
          took: '12m 08s',
        },
        detailLabel: 'Details for the long row',
        defaultOpen: true,
        detail:
          'Runner faber-4471820291-1a2b lost communication with the server while the job was running its test step. The job was queued again and the replacement runner finished it.',
      },
      ...activityRows.slice(0, 2),
    ],
  },
};

/** With `onSelect` each row is pressable; nothing is selected yet. */
export const Selectable: Story = { args: { onSelect: () => {} } };

/** The selected row has the pressed wash and an accent lead bar; pressing another moves it. */
export const SelectedRow: Story = {
  args: { caption: 'Classes' },
  render: (args) => <Picking {...args} first="b" />,
};

/** A selectable row that also has detail keeps its toggle; the toggle does not select. */
export const SelectableWithDetail: Story = {
  args: { caption: 'Recent activity', columns: activityColumns, rows: activityRows },
  render: (args) => <Picking {...args} first="1" />,
};
