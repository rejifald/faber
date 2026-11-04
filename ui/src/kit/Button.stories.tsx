import type { Meta, StoryObj } from '@storybook/react-vite';
import { Pause, RotateCcw, Save } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { ForcedState } from '../../.storybook/force';
import { click } from '../../.storybook/play';
import { Button, type ButtonVariant } from './Button';
import type { Size } from './ladder';
import { Tooltip } from './Tooltip';

// One story per state in Button's @states, plus the ladder steps. Hover, pressed and keyboard
// focus land on one button, the one a pointer or the keyboard would be on, beside a resting one
// of the same variant; hover and pressed get a story per variant, since each variant draws them
// differently.

const meta = { title: 'Kit/Button', component: Button, args: { children: 'Pause' } } satisfies Meta<typeof Button>;
export default meta;
type Story = StoryObj<typeof meta>;

const VARIANTS: ButtonVariant[] = ['primary', 'secondary', 'ghost', 'link', 'row', 'danger'];
const SIZES: Size[] = ['sm', 'md', 'lg'];

const everyVariant = (props: Partial<Parameters<typeof Button>[0]> = {}) =>
  VARIANTS.map((variant) => (
    <Button key={variant} variant={variant} icon={Pause} {...props}>
      {variant}
    </Button>
  ));

/** `state` forced on the first of two buttons of one variant; the second rests. */
const forced = (state: ForcedState, variant: ButtonVariant = 'secondary'): Story => ({
  parameters: { force: { [state]: 'button' } },
  render: () => (
    <>
      <Button variant={variant} icon={Pause}>
        Pause
      </Button>
      <Button variant={variant}>Clear</Button>
    </>
  ),
});

export const Default: Story = { render: () => <>{everyVariant()}</> };

export const HoverPrimary = forced('hover', 'primary');
export const HoverSecondary = forced('hover', 'secondary');
export const HoverGhost = forced('hover', 'ghost');
export const HoverRow = forced('hover', 'row');
export const HoverLink = forced('hover', 'link');
/** A secondary with `tone="danger"`: outlined, red label (Remove before its confirmation). */
export const SecondaryDanger: Story = {
  render: () => (
    <Button variant="secondary" tone="danger">
      Remove
    </Button>
  ),
};

export const HoverDanger = forced('hover', 'danger');

export const PressedPrimary = forced('pressed', 'primary');
export const PressedSecondary = forced('pressed', 'secondary');
export const PressedGhost = forced('pressed', 'ghost');
export const PressedLink = forced('pressed', 'link');

/** The focus ring is the same on every variant. */
export const FocusVisible = forced('focus-visible');

/**
 * How disabled looks, on every variant. A button is disabled only while a sibling action in the
 * same place is in flight (DisabledWhileSiblingRuns); see guidelines § Controls.
 */
export const Disabled: Story = { render: () => <>{everyVariant({ disabled: true })}</> };

/** The one place disabled is right: Reset waits while its sibling, Save settings, runs. */
export const DisabledWhileSiblingRuns: Story = {
  render: () => (
    <>
      <Button variant="primary" icon={Save} loading>
        Save settings
      </Button>
      <Button icon={RotateCcw} disabled>
        Reset
      </Button>
    </>
  ),
};

/**
 * A button that cannot act now stays live: its tooltip previews why, and a press shows the reason
 * inline beside it (guidelines § Controls). The note clears on the next interaction anywhere, or
 * when the condition changes: here, a job starting, which would make Pause able to act.
 */
function PauseWithNothingToPause() {
  const [why, setWhy] = useState(false);
  useEffect(() => {
    if (!why) return;
    const clear = () => setWhy(false);
    const events = ['pointerdown', 'keydown'] as const;
    for (const type of events) document.addEventListener(type, clear, { capture: true, once: true });
    return () => {
      for (const type of events) document.removeEventListener(type, clear, { capture: true });
    };
  }, [why]);
  return (
    <div className="flex items-center gap-sm">
      <Tooltip content="No jobs are running">
        <Button icon={Pause} onClick={() => setWhy(true)}>
          Pause
        </Button>
      </Tooltip>
      {why && (
        <p role="status" className="text-caption text-muted">
          No jobs are running, so there is nothing to pause.
        </p>
      )}
    </div>
  );
}

/** Can't act now, so it stays live and looks ordinary. */
export const Unavailable: Story = { render: () => <PauseWithNothingToPause /> };

/** The same button just pressed: the reason inline beside it, until the next interaction. */
export const UnavailablePressed: Story = {
  render: () => <PauseWithNothingToPause />,
  play: (context) => click(context, 'button', '[role="status"]'),
};

export const Loading: Story = { render: () => <>{everyVariant({ loading: true })}</> };

export const Sizes: Story = {
  render: () => (
    <>
      {SIZES.map((size) => (
        <Button key={size} size={size} icon={Pause}>
          {size}
        </Button>
      ))}
    </>
  ),
};

export const SizesWithoutIcon: Story = {
  render: () => (
    <>
      {SIZES.map((size) => (
        <Button key={size} size={size}>
          {size}
        </Button>
      ))}
    </>
  ),
};
