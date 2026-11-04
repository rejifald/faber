import type { Meta, StoryObj } from '@storybook/react-vite';
import type { Pip, PipKind } from '../popover/groups';
import { StoryCard } from '../popover/StoryCard';
import { PipStrip } from './PipStrip';

// PipStrip's @states, as in reference/pip-glyphs.png and job-row.png.

const meta = { title: 'Blocks/PipStrip', component: PipStrip, args: { pips: [] } } satisfies Meta<typeof PipStrip>;
export default meta;
type Story = StoryObj<typeof meta>;

const pips = (...kinds: PipKind[]): Pip[] => kinds.map((kind, i) => ({ kind, key: `${kind}-${i}` }));

/** faber delivered and GitHub concluded the job, passed or failed: a muted check for 30 s. Held still here; DoneFading shows the fade. */
export const Done: Story = { parameters: { still: true }, args: { pips: [{ kind: 'done', key: 'p0', ageMs: 0 }] } };
/**
 * The fade, played once on load: the pip starts 22 s after its run ended and fades to nothing
 * over the next 8 s. Reload the story to see it again; in the app the next snapshot drops it.
 */
export const DoneFading: Story = { args: { pips: [{ kind: 'done', key: 'p1', ageMs: 22_000 }] } };
export const Running: Story = { args: { pips: pips('running', 'running', 'running') } };
export const Frozen: Story = { args: { pips: pips('frozen', 'frozen') } };
export const Waiting: Story = { args: { pips: pips('waiting') } };
export const Fault: Story = { args: { pips: pips('fault') } };
/** A peer's runners: one busy, two idle (grey rings). A job group never draws idle. */
export const Idle: Story = { args: { pips: pips('running', 'idle', 'idle') } };
/** A peer with 9 runners, 6 busy and 3 idle: idle segments are grey outlines. */
export const OverflowIdle: Story = { args: { pips: pips(...Array<PipKind>(6).fill('running'), 'idle', 'idle', 'idle') } };
/** Finished first (done, fault), then running, frozen, waiting. */
export const Mixed: Story = { args: { pips: pips('done', 'done', 'fault', 'running', 'running', 'frozen') } };
/** Over six: the job count and one segment per job, in strip order. */
export const OverflowBar: Story = { args: { pips: pips(...Array<PipKind>(5).fill('running'), 'frozen', 'waiting', 'waiting', 'waiting') } };
export const OverflowWithFault: Story = { args: { pips: pips('fault', ...Array<PipKind>(5).fill('running'), 'waiting', 'waiting', 'waiting') } };

/** On a hovered row the tint is neutral, so every glyph keeps its colour. */
export const OnHoveredRow: Story = {
  parameters: { force: { hover: '[data-row]' } },
  render: (args) => (
    <StoryCard>
      <span data-row className="group/disclosure flex items-center justify-end bg-row-hover px-xl py-xs">
        <PipStrip {...args} />
      </span>
    </StoryCard>
  ),
  args: { pips: pips('done', 'fault', 'running', 'frozen', 'waiting') },
};

/** The bar on a hovered row: the same colours, filled or outlined, on the neutral lift. */
export const OverflowOnHoveredRow: Story = {
  ...OnHoveredRow,
  args: { pips: pips('done', 'fault', ...Array<PipKind>(4).fill('running'), 'frozen', 'waiting', 'waiting') },
};
/** Many jobs: once a segment per job would be under 4 px (from 11 jobs), each run of one kind shares a segment. */
export const OverflowMany: Story = {
  args: { pips: pips('fault', ...Array<PipKind>(30).fill('running'), ...Array<PipKind>(9).fill('waiting')) },
};

/** Short outlined runs: a lone fault and waiting job among many still show their hollow. */
export const OverflowShortOutlines: Story = {
  args: { pips: pips('fault', ...Array<PipKind>(20).fill('running'), 'frozen', 'waiting') },
};

/** One outlined job in a long bar: a lone waiting job beside 30 running keeps its hollow, so it never reads as frozen. */
export const OverflowLoneOutline: Story = {
  args: { pips: pips('frozen', ...Array<PipKind>(30).fill('running'), 'waiting') },
};

/** Under reduced motion a running pip's arc holds still, and a done pip does not fade. */
export const ReducedMotion: Story = {
  args: { pips: [{ kind: 'done', key: 'p2', ageMs: 25_000 }, ...pips('running', 'running', 'waiting')], still: true },
};

