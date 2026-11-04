import type { Meta, StoryObj } from '@storybook/react-vite';
import { full, NOW, quiet, runFootprint } from '../dashboard/fixtures';
import { Activity } from './Activity';

// Activity (A1) from docs/design/dashboard/reference/B-Activity-A1.png: the main column only, the
// shell draws the sidebar. Stories pass `initialQuery`, so none touches the address bar.

const run = async () => undefined;

const meta = {
  title: 'Screens/Activity',
  component: Activity,
  parameters: { layout: 'fullscreen' },
  args: { state: full, now: NOW, run, initialQuery: '', loadRun: async () => runFootprint },
  decorators: [
    (Story) => (
      <div className="min-h-screen w-full bg-bg p-2xl">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof Activity>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Every kind at once: the full fixture. */
export const Busy: Story = {};

/** Fault chosen in the Kind facet, as `?kind=fault` in the address bar would. */
export const Filtered: Story = { args: { initialQuery: '?kind=fault' } };

/** A row with a message and fields, open: the CodeBlock over the grid. */
export const RowOpen: Story = { args: { openFirstMessage: true } };

/** A finished run open: its phase bar, read from the daemon's run footprint (ADR-0056). */
export const RunPhasesOpen: Story = { args: { openFirstRun: true } };

/** A search that matches nothing: the sentence and Clear filters. */
export const SearchNoMatch: Story = { args: { initialQuery: '?q=nothing-called-this' } };

/** A host that has just started. */
export const Empty: Story = { args: { state: { ...quiet, outcomes: [], recent: [] } } };

/** The last poll is old: the header says Stale and the table dims. */
export const Stale: Story = { args: { stale: true } };
