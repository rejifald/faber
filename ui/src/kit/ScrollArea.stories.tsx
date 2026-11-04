import type { Meta, StoryObj } from '@storybook/react-vite';
import { ScrollArea } from './ScrollArea';

// ScrollArea's @states on the popover card's surface: content that fits, content past its cap,
// and the same with edge fades.

const ROOMY = 200;
const TIGHT = 120;

const lines = Array.from({ length: 14 }, (_, i) => (
  <span key={i} className="px-xl py-3xs text-small text-muted">{`demo/PR-${2700 + i}`}</span>
));

const meta = {
  title: 'Kit/ScrollArea',
  component: ScrollArea,
  args: { children: lines.slice(0, 3) },
  render: (args) => (
    <div className="flex w-popover flex-col rounded-lg border border-border bg-bg py-xs font-sans text-body text-fg">
      <ScrollArea {...args} />
    </div>
  ),
} satisfies Meta<typeof ScrollArea>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Fits: Story = { args: { maxHeight: ROOMY } };
/** Past its cap: the content scrolls. */
export const Scrolls: Story = { args: { children: lines, maxHeight: TIGHT } };
/** Scrolled to the middle, so both edges fade: the top once scrolled, the bottom while more lies below. */
export const ScrolledWithFades: Story = {
  args: { children: lines, maxHeight: TIGHT, fades: true },
  play: async ({ canvasElement }) => {
    const area = canvasElement.querySelector<HTMLElement>('.overflow-y-auto');
    if (!area) throw new Error('no scroll area in the story');
    area.scrollTop = (area.scrollHeight - area.clientHeight) / 2;
    area.dispatchEvent(new Event('scroll'));
  },
};
