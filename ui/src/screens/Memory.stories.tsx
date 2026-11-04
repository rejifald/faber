import type { Meta, StoryObj } from '@storybook/react-vite';
import { NOW, quiet } from '../dashboard/fixtures';
import type { DaemonState, ClassCap } from '../popover/types';
import { Memory } from './Memory';

// Memory (M1) in its states: the busy host, a bimodal class open, a pinned class open, a pin
// saving, a pin failing, no classes yet, and a stale poll. Class names are neutral.

const spread = (n: number, base: number, wobble: number, step: number) => Array.from({ length: n }, (_, i) => base + ((i * step) % wobble));

function cap(job: string, over: Partial<ClassCap>): ClassCap {
  const runner = over.runner_mib ?? 2048;
  const peaks = over.peaks_runner_mib ?? spread(over.samples ?? 30, Math.round(runner * 0.6), Math.round(runner * 0.25), 37);
  return {
    key: `acme/web::1::${job}`,
    label: `acme/web ${job}`,
    runner_mib: runner,
    dind_mib: 256,
    status: 'learned',
    pinned: false,
    samples: peaks.length,
    runner_mcpu: 1500,
    dind_mcpu: 0,
    cpu_status: 'learned',
    cpu_pinned: false,
    reserve_p75_runner_mib: runner,
    reserve_p75_dind_mib: 256,
    reserve_p75_mib: runner + 256,
    p75_runs: 0,
    p75_trips: 0,
    peaks_runner_mib: peaks,
    peaks_dind_mib: spread(peaks.length, 200, 120, 13),
    censored_runner: [],
    censored_dind: [],
    ...over,
  };
}

const bimodalPeaks = [...spread(38, 2900, 500, 37), ...spread(9, 8000, 900, 211)];
const classes: ClassCap[] = [
  cap('e2e', { runner_mib: 6144, status: 'pinned', pinned: true, samples: 89, peaks_runner_mib: spread(89, 5600, 560, 53) }),
  cap('coverage', { runner_mib: 10240, status: 'pinned', pinned: true, samples: 52, peaks_runner_mib: spread(52, 6800, 500, 41) }),
  cap('build', { runner_mib: 2560, status: 'warned', samples: 71, peaks_runner_mib: spread(71, 5600, 1000, 61) }),
  cap('test', { runner_mib: 1024, samples: 143 }),
  cap('lint', { runner_mib: 512, samples: 61 }),
  cap('deploy', { runner_mib: 3072, samples: 47, peaks_runner_mib: bimodalPeaks, censored_runner: bimodalPeaks.map((_, i) => i === 44) }),
  cap('api', { runner_mib: 1536, samples: 34 }),
  cap('infra', { runner_mib: 1792, samples: 3, peaks_runner_mib: [1500, 1700, 1600] }),
];

const busy: DaemonState = {
  ...quiet,
  inflight: [
    { job_id: 1, repo: 'acme/web', job_name: 'e2e', admitted: true, profile: 'linux' },
    { job_id: 2, repo: 'acme/web', job_name: 'test', admitted: true, profile: 'linux' },
    { job_id: 3, repo: 'acme/web', job_name: 'lint', admitted: false, profile: 'linux' },
  ],
  outcomes: [
    { ts_ms: NOW - 360_000, repo: 'acme/web', job_id: 9, job_name: 'deploy' },
    { ts_ms: NOW - 12_000, repo: 'acme/web', job_id: 10, job_name: 'e2e' },
  ],
  budget: {
    ...quiet.budget,
    budget_mib: 16 * 1024,
    reserve_mib: 2 * 1024,
    reserved_mib: 12 * 1024,
    docker_inflight: 3,
    margin: 1.15, // eslint-disable-line design-tokens/no-raw-style-number -- the autolimiter's margin, not a style
    classes,
    warnings: ['acme/web:build is past half the budget: its cap stopped adjusting, pin it to move on'],
    gates: {
      memory: { parked_now: 1, parks: 41, parked_ms: 5_400_000, binding: 38 },
      cpu: { parked_now: 0, parks: 6, parked_ms: 300_000, binding: 5 },
      disk: null,
    },
  },
};

const stub = () => Promise.resolve();
const meta = {
  title: 'Screens/Memory',
  component: Memory,
  args: { state: busy, now: NOW, run: stub },
  parameters: { layout: 'fullscreen' },
  decorators: [(Story) => <div className="min-h-screen w-full bg-bg p-3xl"><Story /></div>],
} satisfies Meta<typeof Memory>;
export default meta;
type Story = StoryObj<typeof meta>;

export const Busy: Story = { args: { initialQuery: '' } };

export const ClassSelected: Story = { args: { initialQuery: '?class=acme/web::1::deploy' } };

export const PinnedClassSelected: Story = { args: { initialQuery: '?class=acme/web::1::e2e' } };

export const PinSaving: Story = { args: { initialQuery: '?class=acme/web::1::deploy', initialPending: true, run: () => new Promise(() => {}) } };

export const PinFailed: Story = { args: { initialQuery: '?class=acme/web::1::deploy', initialError: 'pin refused: the daemon is draining' } };

export const Empty: Story = { args: { initialQuery: '', state: { ...quiet, budget: { ...quiet.budget, classes: [], gates: {}, warnings: [] } } } };

export const Stale: Story = { args: { initialQuery: '', stale: true } };

const BUILD = 'acme/web:build is past half the budget: its cap stopped adjusting, pin it to move on';
const CORES = 'acme/web deploy wants 6 cores, held at 4: pin to allow';
const keyed: DaemonState = {
  ...busy,
  budget: {
    ...busy.budget,
    warnings: [BUILD, CORES],
    warning_items: [
      { key: 'memory:acme/web::1::build', text: BUILD },
      { key: 'cpu:acme/web::1::deploy', text: CORES },
    ],
  },
};
/** A daemon that keys its warnings (ADR-0064): each card carries Dismiss. */
export const WarningDismissable: Story = { args: { initialQuery: '', state: keyed } };
/** One warning dismissed: it folds into "1 dismissed", opened here with its Restore. Its figures moving keeps it hidden. */
export const WarningsDismissed: Story = {
  args: {
    initialQuery: '',
    initialDismissedOpen: true,
    state: { ...keyed, attention: { dismissed: [{ key: 'warning:cpu:acme/web::1::deploy', ident: 'acme/web deploy wants # cores, held at #: pin to allow', at_ms: NOW - 3600_000 }], trust_acked_ms: null } },
  },
};
