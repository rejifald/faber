import type { Meta, StoryObj } from '@storybook/react-vite';
import { PeakHistogram } from './PeakHistogram';

// One story per state in PeakHistogram's @states. Peaks are in MiB.

const warm = [2100, 2300, 2500, 3000, 3100, 3200, 3300, 3400, 2900, 2200];
const bimodal = [...warm, ...Array.from({ length: 28 }, (_, i) => 3100 + (i % 9) * 60), 8300, 8500, 8200, 8700];

const meta = {
  title: 'Kit/PeakHistogram',
  component: PeakHistogram,
  args: { peaks: bimodal, capMib: 3500 },
  decorators: [
    (Story) => (
      <div className="w-rail">
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof PeakHistogram>;
export default meta;
type Story = StoryObj<typeof meta>;

/** A warm cluster and a cold one, with a gap between them; with no split given, every bucket is amber. */
export const Data: Story = {};

/** No peaks yet: only the cap line, and a summary saying so. */
export const NoSamples: Story = { args: { peaks: [] } };

/** Every peak in one bucket. */
export const OneBucket: Story = { args: { peaks: [2100, 2200, 2250, 2400], capMib: 3000 } };

/** The cold-safe value as a second line. */
export const WithColdSafe: Story = { args: { safeMib: 8600 } };

/** A split at the cold cluster: the warm buckets stay amber, the cold ones go red. */
export const WithColdCluster: Story = { args: { coldFromMib: 8192 } };

/** Peaks wholly over the cap carry a red outline, whatever their cluster. */
export const OverTheCap: Story = { args: { peaks: [2100, 2200, 2250, 4300, 4400, 4500], capMib: 3000 } };
