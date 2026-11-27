import type { Meta, StoryObj } from '@storybook/react-vite';
import type { AttentionRow as Row } from '../popover/attention';
import { click } from '../../.storybook/play';
import { StoryCard } from '../popover/StoryCard';
import { AttentionRow } from './AttentionRow';

// AttentionRow's @states (bad, warn, info, with actions, action pending, action failed,
// confirming, dismissable, dismiss pending, dismiss failed, with details, details open), on the card as the
// screen places them: full bleed, no inset.

const bad: Row = {
  key: 'unhealthy',
  sev: 'bad',
  icon: 'alert',
  parts: [{ strong: 'Can’t reach GitHub' }, ' — retrying each cycle. Last: ', { mono: 'GET /repos/owner/demo: 502' }],
};
const warn: Row = { key: 'paused', sev: 'warn', icon: 'pause', parts: [{ strong: 'Paused' }, ' — nothing new starts. Running jobs finish.'] };
const info: Row = { key: 'app-updated', sev: 'info', icon: 'info', parts: [{ strong: 'Updated to 0.3.13' }] };
const staged: Row = {
  key: 'upd-staged',
  sev: 'warn',
  icon: 'update',
  parts: [{ strong: 'Update staged' }, ' — faber 0.3.13 installs when idle.'],
  actions: [
    { label: 'Install now', primary: true, cmd: 'apply_update' },
    { label: 'Skip', cmd: 'skip_update' },
  ],
};
const hold: Row = {
  key: 'repo-hold:owner/demo',
  sev: 'warn',
  icon: 'alert',
  parts: [{ strong: 'Minting held for owner/demo' }, ' — a fork job asked for dind — since 11:50. Clearing needs sudo: ', { mono: 'sudo faber repo-hold clear owner/demo' }],
  actions: [{ label: 'Clear', cmd: 'clear_repo_hold' }],
};
const oomHeld: Row = {
  key: 'oom-held',
  sev: 'bad',
  icon: 'alert',
  parts: [
    { strong: 'OOM-killed on 5 of its last 10 runs' },
    ': ',
    'owner/demo',
    { sep: true },
    'CI',
    { sep: true },
    'test',
    ' — held at 8.5 GiB.',
  ],
  actions: [
    {
      label: 'Pin at 10.5 GiB',
      cmd: 'set_pin',
      confirm: {
        question: 'Pin at 10.5 GiB?',
        consequence: 'Each of its jobs reserves 10.5 GiB, more than faber grants a class on its own, and faber stops learning its size. Unpin it in the panel.',
        label: 'Pin',
      },
    },
  ],
};

const refused: Row = {
  key: 'refused',
  sev: 'warn',
  icon: 'alert',
  parts: [{ strong: '2 jobs refused here' }, ' — no runner of its own; it waits, or takes one minted for another job. First: ', 'owner/demo'],
};
const acked: Row = {
  key: 'trust-acked',
  sev: 'info',
  icon: 'info',
  parts: [{ strong: '2 trust changes this week, acknowledged' }],
  details: {
    label: 'changes',
    lines: [
      [{ strong: 'Trust widened:' }, ' public repos were allowed, from a dashboard session 3 days ago'],
      [{ strong: 'Trust widened:' }, ' a postgres service was declared, from the control socket 2 days ago'],
    ],
  },
};

const meta = { title: 'Blocks/AttentionRow', component: AttentionRow, args: { row: bad } } satisfies Meta<typeof AttentionRow>;
export default meta;
type Story = StoryObj<typeof meta>;

const onCard = (node: React.ReactNode) => <StoryCard inset={false}>{node}</StoryCard>;

export const Bad: Story = { render: (args) => onCard(<AttentionRow {...args} />) };
export const Warn: Story = { args: { row: warn }, render: (args) => onCard(<AttentionRow {...args} />) };
export const Info: Story = { args: { row: info }, render: (args) => onCard(<AttentionRow {...args} />) };
export const WithActions: Story = { args: { row: staged }, render: (args) => onCard(<AttentionRow {...args} />) };
/** The pressed action shows loading; its sibling waits, disabled, until it settles. */
export const ActionPending: Story = { args: { row: staged, pending: 'Install now' }, render: (args) => onCard(<AttentionRow {...args} />) };
export const ActionFailed: Story = {
  args: { row: hold, error: 'Clearing a repo hold needs sudo. Run: sudo faber repo-hold clear owner/demo' },
  render: (args) => onCard(<AttentionRow {...args} />),
};
/** An action that grants more than faber would asks in place first: Cancel (focused), then Pin. */
export const Confirming: Story = {
  args: { row: oomHeld },
  render: (args) => onCard(<AttentionRow {...args} />),
  play: (context) => click(context, 'button', '[role="alertdialog"]'),
};
/** A row that does not stop jobs carries a quiet Dismiss (×) at its end (ADR-0064). */
export const Dismissable: Story = { args: { row: refused, onDismiss: () => {} }, render: (args) => onCard(<AttentionRow {...args} />) };
/** Dismissing: the × spins until the daemon answers. */
export const DismissPending: Story = { args: { row: refused, onDismiss: () => {}, pending: 'Dismiss' }, render: (args) => onCard(<AttentionRow {...args} />) };
/** Dismiss failed: the daemon's answer under the words, the × ready to press again. */
export const DismissFailed: Story = {
  args: { row: refused, onDismiss: () => {}, error: 'faber answered 503' },
  render: (args) => onCard(<AttentionRow {...args} />),
};
/** Acknowledged trust changes, folded into one quiet row that opens to list them. */
export const WithDetails: Story = { args: { row: acked }, render: (args) => onCard(<AttentionRow {...args} />) };
export const DetailsOpen: Story = { args: { row: acked, defaultDetailsOpen: true }, render: (args) => onCard(<AttentionRow {...args} />) };
