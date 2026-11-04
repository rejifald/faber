import type { Meta, StoryObj } from '@storybook/react-vite';
import { Button } from '../kit';
import { StoryCard } from '../popover/StoryCard';
import { LimitRow } from './LimitRow';

// LimitRow's @states on the card, as docs/design/settings/anatomy.md describes them.

const meta = {
  title: 'Blocks/LimitRow',
  component: LimitRow,
  args: {
    label: 'Reserve',
    hint: 'Kept for the VM itself',
    stepper: { value: 2048, display: '2.0 GiB', step: 512, min: 0, onChange: () => {} },
  },
  render: (args) => (
    <StoryCard inset={false}>
      <LimitRow {...args} />
    </StoryCard>
  ),
} satisfies Meta<typeof LimitRow>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Idle: Story = {};
export const Applying: Story = { args: { status: 'applying', stepper: { value: 2560, display: '2.5 GiB', step: 512, min: 0, onChange: () => {} } } };
export const Applied: Story = { args: { label: 'Hard ceiling', hint: 'Most jobs at once', status: 'applied', stepper: { value: 5, display: '5', step: 1, min: 1, onChange: () => {} } } };
export const ErrorWithRetry: Story = {
  args: { label: 'Headroom', hint: 'Left free between jobs', status: 'error', message: 'daemon unreachable', onRetry: () => {} },
};
/** A long reason is cut at 80 characters; the whole text is the line's title. */
export const ErrorLongReason: Story = {
  args: { status: 'error', message: 'reserve_mib 99999 is above the machine’s memory, which is 16384 MiB in total, so it cannot be applied', onRetry: () => {} },
};
export const Warning: Story = {
  args: {
    label: 'Hard ceiling',
    hint: 'Most jobs at once',
    status: 'warning',
    message: 'Above what CPU allows. CPU binds at 6.',
    stepper: { value: 8, display: '8', step: 1, min: 1, onChange: () => {} },
  },
};
/** The *Use auto* link button after the hint, once the ceiling is explicit. */
export const WithExtra: Story = {
  args: {
    label: 'Hard ceiling',
    hint: 'Most jobs at once',
    stepper: { value: 5, display: '5', step: 1, min: 1, onChange: () => {} },
    extra: (
      <Button size="sm" variant="link">
        Use auto
      </Button>
    ),
  },
};
