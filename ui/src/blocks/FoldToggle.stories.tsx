import type { Meta, StoryObj } from '@storybook/react-vite';
import { Info, TriangleAlert } from 'lucide-react';
import { useState } from 'react';
import { Icon } from '../kit';
import { StoryCard } from '../popover/StoryCard';
import { PIP_WORD, type PipKind } from '../popover/groups';
import { FoldToggle, type FoldToggleProps } from './FoldToggle';
import { PipStrip } from './PipStrip';

// FoldToggle's @states, on the card as the jobs column places them: full bleed, no inset. It is
// controlled by the screen, so each story keeps its own open state, as the screen does.

const pip = (kind: PipKind, count: number) => ({
  key: kind,
  count,
  label: PIP_WORD[kind],
  mark: <PipStrip pips={[{ kind, key: kind }]} still />,
});

const running = [pip('running', 3), pip('waiting', 2), pip('done', 6)];

const meta = {
  title: 'Blocks/FoldToggle',
  component: FoldToggle,
  parameters: { still: true },
  args: { count: 11, open: false, onOpenChange: () => {}, tally: running },
  render: (args) => <Controlled {...args} />,
} satisfies Meta<typeof FoldToggle>;
export default meta;
type Story = StoryObj<typeof meta>;

function Controlled(args: FoldToggleProps) {
  const [open, setOpen] = useState(args.open);
  return (
    <StoryCard inset={false}>
      <FoldToggle {...args} open={open} onOpenChange={setOpen} />
    </StoryCard>
  );
}

export const Collapsed: Story = {};
/** A runner failure folded: its red disc leads the tally, so the failure is never only a count. */
export const CollapsedWithFailure: Story = { args: { count: 12, tally: [pip('fault', 1), ...running] } };
/** The attention list's tally: one severity icon per kind, in the rows' own tones. */
export const CollapsedSeverities: Story = {
  args: {
    count: 2,
    tally: [
      { key: 'warn', count: 1, label: 'warning', mark: <span className="flex text-amber"><Icon icon={TriangleAlert} size="sm" /></span> },
      { key: 'info', count: 1, label: 'notice', mark: <span className="flex text-muted"><Icon icon={Info} size="sm" /></span> },
    ],
  },
};
export const Open: Story = { args: { open: true } };
export const OpenWithPanelLink: Story = { args: { open: true, onPanel: () => {} } };
export const Hover: Story = { parameters: { still: true, force: { hover: 'button' } } };
export const Pressed: Story = { parameters: { still: true, force: { pressed: 'button' } } };
export const FocusVisible: Story = { parameters: { still: true, force: { 'focus-visible': 'button' } } };
