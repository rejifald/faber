import type { Meta, StoryObj } from '@storybook/react-vite';
import { RowDisclosure } from './RowDisclosure';

// RowDisclosure's @states (closed, hover, focus-visible, open), drawn inside a real table.

function Example({ defaultOpen = false }: { defaultOpen?: boolean }) {
  return (
    <table className="w-full border-collapse text-left text-body text-fg">
      <caption className="sr-only">Recent activity</caption>
      <RowDisclosure
        label="Details for acme/mobile build"
        defaultOpen={defaultOpen}
        colSpan={3}
        row={(toggle) => (
          <tr>
            <td className="w-px border-t border-border-soft px-lg py-md">{toggle}</td>
            <td className="border-t border-border-soft px-lg py-md">acme/mobile</td>
            <td className="border-t border-border-soft px-lg py-md">build</td>
          </tr>
        )}
        detail="The runner stayed idle past its 3 h limit and was removed."
      />
    </table>
  );
}

const meta = {
  title: 'Kit/RowDisclosure',
  component: RowDisclosure,
  args: { label: '', row: () => null, detail: null, colSpan: 3 },
  render: () => <Example />,
} satisfies Meta<typeof RowDisclosure>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Closed: Story = {};
export const Hover: Story = { parameters: { force: { hover: 'button' } } };
export const FocusVisible: Story = { parameters: { force: { 'focus-visible': 'button' } } };
export const Open: Story = { render: () => <Example defaultOpen /> };
