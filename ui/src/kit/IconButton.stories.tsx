import type { Meta, StoryObj } from '@storybook/react-vite';
import { RefreshCw, Trash2 } from 'lucide-react';
import type { ForcedState } from '../../.storybook/force';
import { hover } from '../../.storybook/play';
import type { ButtonVariant } from './Button';
import { IconButton } from './IconButton';
import type { Size } from './ladder';

// One story per state in IconButton's @states, plus the ladder steps and its tooltip open. Hover,
// pressed and keyboard focus land on one button beside a resting one of the same variant.

const meta = {
  title: 'Kit/IconButton',
  component: IconButton,
  args: { icon: RefreshCw, label: 'Reconcile now' },
} satisfies Meta<typeof IconButton>;
export default meta;
type Story = StoryObj<typeof meta>;

const VARIANTS: ButtonVariant[] = ['ghost', 'secondary', 'primary'];
const SIZES: Size[] = ['sm', 'md', 'lg'];

const everyVariant = (props: { disabled?: boolean; loading?: boolean } = {}) =>
  VARIANTS.map((variant) => (
    <IconButton key={variant} icon={RefreshCw} label={`Reconcile now (${variant})`} variant={variant} {...props} />
  ));

/** `state` forced on the first of two icon buttons of one variant; the second rests. */
const forced = (state: ForcedState, variant: ButtonVariant = 'ghost'): Story => ({
  parameters: { force: { [state]: 'button' } },
  render: () => (
    <>
      <IconButton icon={RefreshCw} label="Reconcile now" variant={variant} />
      <IconButton icon={Trash2} label="Clear finished jobs" variant={variant} />
    </>
  ),
});

export const Default: Story = { render: () => <>{everyVariant()}</> };

export const HoverGhost = forced('hover', 'ghost');
export const HoverSecondary = forced('hover', 'secondary');
export const HoverPrimary = forced('hover', 'primary');

export const PressedGhost = forced('pressed', 'ghost');
export const PressedSecondary = forced('pressed', 'secondary');
export const PressedPrimary = forced('pressed', 'primary');

export const FocusVisible = forced('focus-visible');

/**
 * How disabled looks. An icon button is disabled only while a sibling action in the same place
 * is in flight; otherwise it stays live and says why on press (guidelines § Controls).
 */
export const Disabled: Story = { render: () => <>{everyVariant({ disabled: true })}</> };

export const Loading: Story = { render: () => <>{everyVariant({ loading: true })}</> };

export const Sizes: Story = {
  render: () => (
    <>
      {SIZES.map((size) => (
        <IconButton key={size} size={size} icon={RefreshCw} label={`Reconcile now (${size})`} variant="secondary" />
      ))}
    </>
  ),
};

/** Its label, shown as the tooltip on hover. */
export const TooltipOpen: Story = { play: (context) => hover(context, 'button') };
