import type { Meta, StoryObj } from '@storybook/react-vite';
import * as fx from '../popover/fixtures';
import type { RowState } from '../popover/limits';
import type { Invoke } from '../popover/types';
import { StoryCard } from '../popover/StoryCard';
import { LimitsView } from './LimitsView';

// The Limits view's @states on the popover card, as docs/design/settings/anatomy.md
// describes them. The header and ⋯ menu above it belong to Screens/Popover (LimitsOpen).

const invoke: Invoke = async () => undefined;
const row = (over: Partial<RowState>): RowState => ({ shown: null, arg: null, status: 'idle', seq: 0, ...over });

const meta = {
  title: 'Screens/LimitsView',
  component: LimitsView,
  args: { snap: fx.limits(), invoke, onBack: () => {}, onOpenSettings: () => {}, onReconnect: () => {} },
  render: (args) => (
    <StoryCard inset={false}>
      <LimitsView {...args} />
    </StoryCard>
  ),
} satisfies Meta<typeof LimitsView>;
export default meta;
type Story = StoryObj<typeof meta>;

/** reference D1 (b): the bar, five rows, All settings…. */
export const Loaded: Story = {};

/** Reserve pressed to 2.5 GiB: the value shows at once and the request goes after 400 ms. */
export const Applying: Story = { args: { initialRows: { reserve: row({ shown: 2560, arg: 2560, status: 'applying', seq: 1 }) } } };

/** The change took: "Applied" with the green border, for 2 s. */
export const Applied: Story = { args: { initialRows: { ceiling: row({ shown: 5, arg: 5, status: 'applied', seq: 1 }) } } };

/** One row failed: it keeps the daemon's reason and a Retry; the others stay live. */
export const Error: Story = {
  args: { initialRows: { headroom: row({ shown: 1536, arg: 1536, status: 'error', message: 'daemon unreachable', seq: 1 }) } },
};

/** CPU binds at 4 (2 running and 2 more fit), and the ceiling is 8. */
export const CpuWarning: Story = {
  args: { snap: fx.limits({ hard_ceiling: 8, hard_ceiling_setting: 8, headroom: { jobs: 2, binds: 'cpu' } }) },
};

/** Auto: the resolved number and "Auto, faber’s pick"; the first press makes it explicit. */
export const CeilingAuto: Story = { args: { snap: fx.limits({ hard_ceiling: 6, hard_ceiling_setting: 'auto' }) } };

/** Off: "Off" and "Budget only"; + sets the resolved number, or 1 when there is none. */
export const CeilingOff: Story = { args: { snap: fx.limits({ hard_ceiling: null, hard_ceiling_setting: 'off' }) } };

/** reference D1 (c, last card): the daemon is not answering. */
export const Unavailable: Story = { args: { snap: fx.offline } };

/** A daemon that answers but predates the limit fields: no Reconnect, since it cannot help. */
export const OlderDaemon: Story = {
  args: { snap: { ...fx.limits(), state: { ...fx.limits().state, budget: { budget_mib: 16384, reserved_mib: 0 } } } as typeof fx.idle },
};

/** Before the first snapshot: the rows hold "…" and their steppers are busy. */
export const Loading: Story = { args: { snap: null } };
