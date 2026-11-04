import type { Meta, StoryObj } from '@storybook/react-vite';
import { EmptyProfiles } from './EmptyProfiles';

// EmptyProfiles as docs/design/settings/anatomy.md describes it.

const meta = { title: 'Blocks/EmptyProfiles', component: EmptyProfiles, args: { onAdd: () => {} } } satisfies Meta<typeof EmptyProfiles>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = { render: (args) => <div className="w-popover font-sans"><EmptyProfiles {...args} /></div> };

/** Unsaved changes: the save bar's Save is the one primary, so Add profile is the outlined secondary. */
export const Dirty: Story = { args: { dirty: true }, render: Default.render };
