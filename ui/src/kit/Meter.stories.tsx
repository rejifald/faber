import type { Meta, StoryObj } from "@storybook/react-vite";
import { Meter } from "./Meter";

// One story per state in Meter's @states. Meter draws nothing, so each story draws a simple bar.

const meta = {
  title: "Kit/Meter",
  component: Meter,
  args: {
    value: 12,
    max: 18,
    label: "Memory budget",
    valueText: "12.0 of 18.0 GiB reserved",
    children: null,
  },
  render: (args) => (
    <Meter {...args}>
      <div className="h-rail w-64 rounded-xs bg-surface-2">
        <div
          className="h-full rounded-xs bg-accent"
          style={{ width: `${Math.min(100, (args.value / args.max) * 100)}%` }}
        />
      </div>
    </Meter>
  ),
} satisfies Meta<typeof Meter>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

export const Full: Story = {
  args: { value: 18, valueText: "18.0 of 18.0 GiB reserved" },
};

/** The ARIA value is clamped to max; the spoken text keeps the real figure. */
export const OverMax: Story = {
  args: { value: 20, valueText: "20.0 of 18.0 GiB reserved, over budget" },
};
