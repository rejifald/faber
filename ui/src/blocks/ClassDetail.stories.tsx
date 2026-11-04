import type { Meta, StoryObj } from '@storybook/react-vite';
import { NOW, quiet } from '../dashboard/fixtures';
import { classViews, type ClassView } from '../dashboard/memory';
import type { ClassCap } from '../popover/types';
import { ClassDetail } from './ClassDetail';

// ClassDetail's @states (learned, pinned, warned, bimodal, still learning, saving a pin, pin
// failed): Memory's side panel at the rail's width.

const spread = (n: number, base: number, wobble: number, step: number) => Array.from({ length: n }, (_, i) => base + ((i * step) % wobble));

function cap(over: Partial<ClassCap> & Pick<ClassCap, 'key' | 'status'>): ClassCap {
  return {
    label: over.key,
    runner_mib: 3072,
    dind_mib: 512,
    pinned: false,
    samples: 40,
    runner_mcpu: 1500,
    dind_mcpu: 0,
    cpu_status: 'learned',
    cpu_pinned: false,
    reserve_p75_runner_mib: 3072,
    reserve_p75_dind_mib: 512,
    reserve_p75_mib: 3584,
    p75_runs: 0,
    p75_trips: 0,
    peaks_runner_mib: spread(40, 2600, 500, 37),
    peaks_dind_mib: spread(40, 300, 150, 11),
    censored_runner: [],
    censored_dind: [],
    ...over,
  };
}

// The autolimiter's safety margin, not a style.
const MARGIN = 1.15;

function view(c: ClassCap): ClassView {
  const state = { ...quiet, budget: { ...quiet.budget, margin: MARGIN, classes: [c] }, inflight: [], outcomes: [{ ts_ms: NOW - 360_000, repo: 'acme/web', job_id: 1, job_name: 'build' }] };
  return classViews(state, NOW)[0];
}

const learned = view(cap({ key: 'acme/web::1::build', status: 'learned' }));
const bimodalPeaks = [...spread(38, 2900, 500, 37), ...spread(9, 8000, 900, 211)];
const bimodal = view(cap({ key: 'acme/web::1::build', status: 'learned', samples: 47, peaks_runner_mib: bimodalPeaks, peaks_dind_mib: spread(47, 300, 150, 11), censored_runner: bimodalPeaks.map((_, i) => i === 44) }));

const meta = {
  title: 'Blocks/ClassDetail',
  component: ClassDetail,
  args: { cls: learned, onClose: () => {}, onPin: () => {}, onUnpin: () => {} },
  decorators: [(Story) => <div className="w-rail"><Story /></div>],
} satisfies Meta<typeof ClassDetail>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Learned: Story = {};

export const Pinned: Story = {
  args: { cls: view(cap({ key: 'acme/web::1::build', status: 'pinned', pinned: true, runner_mib: 6144, dind_mib: 512 })) },
};

/** Past half the budget: faber stopped adjusting, the peaks run over the cap and a pin is the way on. */
export const Warned: Story = {
  args: { cls: view(cap({ key: 'acme/web::1::build', status: 'warned', runner_mib: 2457, dind_mib: 307, peaks_runner_mib: spread(40, 4200, 2200, 97) })) },
};

export const Bimodal: Story = { args: { cls: bimodal } };

export const StillLearning: Story = {
  args: { cls: view(cap({ key: 'acme/web::1::build', status: 'profiling', samples: 3, peaks_runner_mib: [1500, 1700, 1600], peaks_dind_mib: [300, 320, 280] })) },
};

export const NoSamples: Story = {
  args: { cls: view(cap({ key: 'acme/web::1::build', status: 'profiling', samples: 0, peaks_runner_mib: [], peaks_dind_mib: [] })) },
};

export const SavingPin: Story = { args: { cls: bimodal, pending: true } };

export const PinFailed: Story = { args: { cls: bimodal, error: 'pin refused: the daemon is draining' } };
