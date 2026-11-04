import type { Meta, StoryObj } from '@storybook/react-vite';
import { StatusDot, type StatusTone } from './StatusDot';

// One story per state in StatusDot's @states: each tone, and the pulse.

const meta = { title: 'Kit/StatusDot', component: StatusDot, args: { tone: 'green' } } satisfies Meta<typeof StatusDot>;
export default meta;
type Story = StoryObj<typeof meta>;

const withText = (tone: StatusTone, text: string, pulse = false) => (
  <span key={tone} className="inline-flex items-center gap-xs text-small text-fg">
    <StatusDot tone={tone} pulse={pulse} />
    {text}
  </span>
);

export const Green: Story = { render: () => withText('green', 'Online') };
export const Amber: Story = { render: () => withText('amber', 'Degraded') };
export const Red: Story = { render: () => withText('red', 'Offline') };
export const Neutral: Story = { render: () => withText('neutral', 'Idle') };
export const Accent: Story = { render: () => withText('accent', 'Running') };

/** Running: a ring pulses outward. Still when the reader prefers reduced motion. */
export const Pulse: Story = { render: () => withText('accent', 'Running', true) };

/** A state in transition: a hollow dot, drawn as a stroke. */
export const Ring: Story = {
  render: () => (
    <span className="inline-flex items-center gap-xs text-small text-fg">
      <StatusDot tone="amber" ring />
      Booting
    </span>
  ),
};

/** Alone, the dot is named with `label` and exposed as an image. */
export const Labelled: Story = { args: { tone: 'red', label: 'Offline' } };
