import type { Meta, StoryObj } from '@storybook/react-vite';
import { AttentionCard } from './AttentionCard';

// AttentionCard's @states (amber, red, neutral, with actions, action pending, action failed,
// dismissable, dismiss pending, dismiss failed, with details, details open).

const decide = [
  { label: 'Pin to allow', onClick: () => {} },
  { label: 'Dismiss', onClick: () => {} },
];
const measured = (
  <>
    <span className="font-mono text-caption">acme/mobile:build</span> measured 6.4 GiB against a 4 GiB cap. Pin the class to
    allow it, or dismiss to keep the cap.
  </>
);

const meta = {
  title: 'Blocks/AttentionCard',
  component: AttentionCard,
  args: { tone: 'amber', title: 'Needs a decision', body: measured },
  decorators: [(Story) => <div className="w-rail"><Story /></div>],
} satisfies Meta<typeof AttentionCard>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Amber: Story = {};
export const Red: Story = {
  args: {
    tone: 'red',
    title: 'Can’t reach GitHub',
    body: 'Retrying each cycle. Last: GET /repos/acme/web: 502',
  },
};
export const Neutral: Story = {
  args: { tone: 'neutral', title: 'Updated to 0.3.13', body: 'The app restarted on the new version at 11:50.' },
};
export const WithActions: Story = { args: { actions: decide } };
/** The pressed action shows loading; its sibling waits, disabled, until it settles. */
export const ActionPending: Story = { args: { actions: decide, pending: 'Pin to allow' } };
export const ActionFailed: Story = {
  args: { actions: decide, error: 'Pinning needs sudo. Run: sudo faber pin acme/mobile:build' },
};
/** An issue that does not stop jobs: a quiet Dismiss (×) beside the title (ADR-0064). */
export const Dismissable: Story = {
  args: { title: 'Needs attention', body: 'acme/mobile build wants 6 cores, held at 4: pin to allow', onDismiss: () => {} },
};
export const DismissPending: Story = {
  args: { title: 'Needs attention', body: 'acme/mobile build wants 6 cores, held at 4: pin to allow', onDismiss: () => {}, pending: 'Dismiss' },
};
export const DismissFailed: Story = {
  args: { title: 'Needs attention', body: 'acme/mobile build wants 6 cores, held at 4: pin to allow', onDismiss: () => {}, error: 'faber answered 503' },
};
const changes = {
  label: 'changes',
  lines: ['Trust widened: public repos were allowed, from a dashboard session 3 days ago', 'Trust widened: a postgres service was declared, from the control socket 2 days ago'],
};
/** Acknowledged trust changes, folded into one quiet card that opens to list them. */
export const WithDetails: Story = { args: { tone: 'neutral', title: '2 trust changes this week, acknowledged', body: null, details: changes } };
export const DetailsOpen: Story = {
  args: { tone: 'neutral', title: '2 trust changes this week, acknowledged', body: null, details: changes, defaultDetailsOpen: true },
};
