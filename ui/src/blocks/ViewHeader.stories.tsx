import type { Meta, StoryObj } from '@storybook/react-vite';
import { Button } from '../kit';
import { StoryCard } from '../popover/StoryCard';
import { ViewHeader } from './ViewHeader';

// ViewHeader as the Limits strip draws it (settings/anatomy.md § 5).

const meta = { title: 'Blocks/ViewHeader', component: ViewHeader, args: { title: 'Limits' } } satisfies Meta<typeof ViewHeader>;
export default meta;
type Story = StoryObj<typeof meta>;

const appliesNow = (
  <>
    <span aria-hidden className="size-md rounded-full bg-green" />
    Applies now
  </>
);

const strip = () => (
  <StoryCard inset={false}>
    <ViewHeader title="Limits" onBack={() => {}} status={appliesNow} />
  </StoryCard>
);

export const Strip: Story = { render: strip };
export const BackHover: Story = { parameters: { force: { hover: 'button[aria-label="Back"]' } }, render: strip };
export const BackFocus: Story = { parameters: { force: { 'focus-visible': 'button[aria-label="Back"]' } }, render: strip };

/** The Settings view's profile page: "Profiles › name" with Remove at the right. */
export const Crumb: Story = {
  render: () => (
    <StoryCard inset={false}>
      <ViewHeader title="linux-docker" crumb={{ label: 'Profiles', onClick: () => {} }} action={<Button size="sm">Remove</Button>} />
    </StoryCard>
  ),
};
