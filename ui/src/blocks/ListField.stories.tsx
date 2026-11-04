import type { Meta, StoryObj } from '@storybook/react-vite';
import { fields, profileFields } from '../settings/fixtures';
import { ListField } from './ListField';

// ListField's @states as Trust.png draws them, and as docs/design/settings/anatomy.md describes the other lists.

const meta = {
  title: 'Blocks/ListField',
  component: ListField,
  args: { field: fields.events, items: ['push', 'pull_request', 'workflow_dispatch'], loaded: ['push', 'pull_request', 'workflow_dispatch'], onChange: () => {} },
  render: (args) => (
    <div className="w-label bg-surface p-xl font-sans text-body text-fg">
      <ListField {...args} />
    </div>
  ),
} satisfies Meta<typeof ListField>;
export default meta;
type Story = StoryObj<typeof meta>;

const drop = profileFields.find((p) => p.key === 'drop_grants')!;

/** Narrows `remove`: a remove button on each item, the add command below. */
export const Remove: Story = {};
export const ItemRemoved: Story = { args: { items: ['push', 'workflow_dispatch'], loaded: ['push', 'pull_request', 'workflow_dispatch'] } };
export const EveryItemRemoved: Story = { args: { items: [], loaded: ['push', 'pull_request'] } };
/** Narrows `remove-keep-one`: removing the last item is refused with the note. Press the last item's remove. */
export const KeepOne: Story = { args: { field: fields.trusted, items: ['owner', 'release-bot'], loaded: ['owner', 'release-bot'] } };
/** Narrows `add`: an add row and no remove buttons, the remove command below. */
export const AddOnly: Story = { args: { field: fields.denied, items: ['someone-else'], loaded: ['someone-else'] } };
export const ItemAdded: Story = { args: { field: drop, items: ['192.0.2.7/32 3306/tcp'], loaded: [], profile: { name: 'linux-docker' } } };
export const Empty: Story = { args: { field: fields.denied, items: [], loaded: [], empty: 'No denied actors. Add a login to refuse its jobs on every repo.' } };
/** Free list: remove on every item, an add row. */
export const Free: Story = { args: { field: fields.priority, items: ['owner/demo', 'owner/docs'], loaded: ['owner/demo', 'owner/docs'] } };
