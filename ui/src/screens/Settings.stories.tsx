import type { Meta, StoryObj } from '@storybook/react-vite';
import { settingsView } from '../settings/fixtures';
import { PROFILES_ID, RAW_ID } from '../settings/nav';
import type { Config, SettingsView } from '../settings/types';
import { Settings, newProfile, type SettingsApi, type SettingsProps } from './Settings';

// Every state of the Settings view (anatomy.md § 5, Settings and RawEditor), at the panel window's
// 460 px and at 1024 px. Below 640 px the nav is a Select above the form (§ 6). The Storybook theme
// switch covers dark. The page is seeded through `initial`, so no daemon is needed.

const clone = <T,>(v: T): T => structuredClone(v);
const never = <T,>() => new Promise<T>(() => {});
const api: SettingsApi = {
  load: async () => settingsView,
  save: () => never(),
  loadRaw: () => never(),
  saveRaw: () => never(),
};
const offline: SettingsApi = { ...api, load: async () => Promise.reject(new Error('faber didn’t answer on its control socket.')) };
const hanging: SettingsApi = { ...api, load: () => never() };

const edited = (fn: (c: Config) => void): Config => {
  const c = clone(settingsView.config);
  fn(c);
  return c;
};
const withView = (over: Partial<SettingsView>): SettingsView => ({ ...settingsView, ...over });
const noProfiles = withView({ config: edited((c) => void delete c.profile) });

const RAW_TEXT = [
  '# faber: watched repos carry the topic below.',
  'topic = "faber"',
  'poll_interval_secs = 30',
  'sweep_interval_secs = 600',
  '',
  '[autolimit]',
  'reserve_mib = 2048',
  'headroom_mib = 1536',
  'margin = 1.2',
  'hard_ceiling = 4',
  '',
  '[gate]',
  'allowed_events = ["push", "pull_request", "workflow_dispatch"]',
  'allow_public_repos = true',
  '',
  '[[profile]]',
  'name = "linux-docker"',
  'labels = ["faber", "docker"]',
  'backend = "docker"',
  '',
].join('\n');
const RAW_FILE = { path: settingsView.path, text: RAW_TEXT, base: 'abc123' };
const raw = (over: Partial<NonNullable<SettingsProps['initial']>['raw']> = {}): NonNullable<SettingsProps['initial']> => ({
  view: settingsView,
  section: RAW_ID,
  raw: { file: RAW_FILE, text: RAW_TEXT, loading: false, saving: false, ...over },
});

const meta = {
  title: 'Screens/Settings',
  component: Settings,
  args: { api, initial: { view: settingsView } },
  parameters: {
    layout: 'fullscreen',
    viewport: {
      options: {
        panel: { name: 'Panel window 460', styles: { width: '460px', height: '640px' } }, // eslint-disable-line design-tokens/no-raw-style-number -- a story's viewport frame, not UI
        wide: { name: 'Browser tab 1024', styles: { width: '1024px', height: '720px' } }, // eslint-disable-line design-tokens/no-raw-style-number -- a story's viewport frame, not UI
      },
    },
  },
  globals: { viewport: { value: 'panel', isRotated: false } },
} satisfies Meta<typeof Settings>;
export default meta;
type Story = StoryObj<typeof meta>;

/** The same story in a browser tab: the sidebar beside the form. */
const wide = (s: Story): Story => ({ ...s, globals: { viewport: { value: 'wide', isRotated: false } } });

/** Before the first answer: skeleton rows. */
export const Loading: Story = { args: { api: hanging, initial: {} } };
export const Loading1024 = wide(Loading);

/** Limits: the live fields, tagged live, on the nav's first item. */
export const Loaded: Story = { args: { initial: { view: settingsView, section: 'limits' } } };
export const Loaded1024 = wide(Loaded);

/** Admission tuning: the Autolimiter's other fields, tagged restart. */
export const AdmissionTuning: Story = { args: { initial: { view: settingsView, section: 'admission-tuning' } } };
export const AdmissionTuning1024 = wide(AdmissionTuning);

/** Trust with one event removed, unsaved (Trust.png). */
export const TrustRemovalPending: Story = {
  args: {
    initial: { view: settingsView, section: 'trust', draft: edited((c) => ((c.gate as Config).allowed_events = ['push', 'pull_request', 'workflow_dispatch'])) },
  },
};
export const TrustRemovalPending1024 = wide(TrustRemovalPending);

/** Two edits, one live and one on restart: the raised save bar names both. */
export const Unsaved: Story = {
  args: {
    initial: {
      view: settingsView,
      section: 'limits',
      draft: edited((c) => ((c.autolimit as Config).headroom_mib = 1024)),
    },
  },
};
export const Unsaved1024 = wide(Unsaved);

/** The save is in flight: Save loads and Discard waits. */
export const Saving: Story = { args: { initial: { ...Unsaved.args!.initial, saving: true } } };
export const Saving1024 = wide(Saving);

/** Saved, with a change that waits for a restart. */
export const SavedRestartPending: Story = {
  args: { initial: { view: withView({ pending_restart: ['autolimit.cpu_overcommit'] }), section: 'admission-tuning', justSaved: true } },
};
export const SavedRestartPending1024 = wide(SavedRestartPending);

/** A 400: the field the daemon names shows its message under the control. */
export const Invalid: Story = {
  args: {
    initial: {
      ...Unsaved.args!.initial,
      outcome: { kind: 'invalid', errors: ['Headroom must be a whole number of MiB.'] },
      fieldErrors: { 'autolimit.headroom_mib': 'Headroom must be a whole number of MiB.' },
    },
  },
};
export const Invalid1024 = wide(Invalid);

/** A 403: a widening, refused with its sentence and the sudo command. */
export const Refused: Story = {
  args: {
    initial: {
      view: settingsView,
      section: 'trust',
      draft: edited((c) => ((c.gate as Config).allow_public_repos = true)),
      outcome: {
        kind: 'refused',
        refused: [
          {
            key: 'gate.allow_public_repos',
            sentence: 'allow_public_repos turns on, so anyone can queue jobs against a watched public repo.',
            needs: 'root',
            command: 'sudo faber config apply config.toml',
          },
        ],
      },
    },
  },
};
export const Refused1024 = wide(Refused);

/** A 409: the file changed elsewhere; the new one is loaded and the draft dropped. */
export const Stale: Story = { args: { initial: { view: settingsView, section: 'limits', outcome: { kind: 'stale' } } } };
export const Stale1024 = wide(Stale);

/** GET /api/settings failed: the pane says so with Retry. */
export const CantLoad: Story = { args: { api: offline, initial: { phase: 'error', loadError: 'faber didn’t answer on its control socket.' } } };
export const CantLoad1024 = wide(CantLoad);

/** A call answered 401: the session is gone, so the pane says why and offers no retry. */
export const SessionEnded: Story = { args: { initial: { view: settingsView, section: 'limits', sessionEnded: 'The daemon restarted (an update or a restart)' } } };
export const SessionEnded1024 = wide(SessionEnded);

/** A daemon started without a config file: it claims no jobs until a profile is saved (ADR-0052). */
export const FromFlags: Story = { args: { initial: { view: withView({ from_flags: true }), section: 'limits' } } };
export const FromFlags1024 = wide(FromFlags);

/** Profiles: the list, in routing order. */
export const ProfilesList: Story = { args: { initial: { view: settingsView, section: PROFILES_ID } } };
export const ProfilesList1024 = wide(ProfilesList);

/** One profile after Revoke on Docker access. */
export const ProfileOpen: Story = {
  args: {
    initial: {
      view: settingsView,
      section: PROFILES_ID,
      profile: 0,
      draft: edited((c) => ((c.profile as Config[])[0].dind = false)),
    },
  },
};
export const ProfileOpen1024 = wide(ProfileOpen);

/** A macOS VM profile: the Virtual machine fields appear. */
export const ProfileMacosVm: Story = { args: { initial: { view: settingsView, section: PROFILES_ID, profile: 2 } } };
export const ProfileMacosVm1024 = wide(ProfileMacosVm);

/** Add profile: a blank name and a duplicate one is refused before saving. */
export const ProfileNew: Story = {
  args: {
    initial: {
      view: settingsView,
      section: PROFILES_ID,
      profile: 3,
      draft: edited((c) => ((c.profile as Config[]).push({ ...newProfile(), name: 'linux' }))),
      fieldErrors: { 'profile.3.name': 'A profile called linux already exists.' },
    },
  },
};
export const ProfileNew1024 = wide(ProfileNew);

/** Remove asks in place before anything is saved. */
export const ProfileRemoveConfirm: Story = {
  args: { initial: { view: settingsView, section: PROFILES_ID, profile: 1, confirmRemove: true } },
};
export const ProfileRemoveConfirm1024 = wide(ProfileRemoveConfirm);

/** No profile in the config: faber cannot take jobs until it has one. */
export const NoProfiles: Story = { args: { initial: { view: noProfiles, section: PROFILES_ID } } };
export const NoProfiles1024 = wide(NoProfiles);

/** Every profile removed, not yet saved: the save bar's Save is the one primary, Add profile is outlined. */
export const NoProfilesUnsaved: Story = {
  args: { initial: { view: settingsView, section: PROFILES_ID, draft: edited((c) => void delete c.profile) } },
};
export const NoProfilesUnsaved1024 = wide(NoProfilesUnsaved);

/** config.toml, clean (Raw.png). */
export const RawClean: Story = { args: { initial: raw() } };
export const RawClean1024 = wide(RawClean);

/** Two lines changed: tinted, and the save bar says what applies now and on restart. */
export const RawChanged: Story = {
  args: {
    initial: raw({
      text: RAW_TEXT.replace('sweep_interval_secs = 600', 'sweep_interval_secs = 900').replace('headroom_mib = 1536', 'headroom_mib = 1024'),
      preview: [
        { key: 'autolimit.headroom_mib', label: 'Headroom', effect: 'live' },
        { key: 'sweep_interval_secs', label: 'Sweep interval', effect: 'restart' },
      ],
    }),
  },
};
export const RawChanged1024 = wide(RawChanged);

/** A TOML error: its line is red and "Line N: message" shows. */
export const RawTomlError: Story = {
  args: {
    initial: raw({
      text: RAW_TEXT.replace('headroom_mib = 1536', 'headroom_mib = 1536 MiB'),
      error: { line: 8, column: 20, message: 'expected a number after headroom_mib =' },
    }),
  },
};
export const RawTomlError1024 = wide(RawTomlError);

/** A value faber rejects: the daemon's reason for the line. */
export const RawRejected: Story = {
  args: {
    initial: raw({
      text: RAW_TEXT.replace('hard_ceiling = 4', 'hard_ceiling = 0'),
      outcome: { kind: 'invalid', errors: ['Line 10: hard_ceiling must be at least 1, "auto" or "off".'] },
    }),
  },
};
export const RawRejected1024 = wide(RawRejected);

/** The text widens trust: refused, nothing written. */
export const RawRefused: Story = {
  args: {
    initial: raw({
      text: RAW_TEXT.replace('allow_public_repos = true', 'allow_public_repos = true # widened'),
      outcome: {
        kind: 'refused',
        refused: [
          {
            key: 'gate.allow_public_repos',
            sentence: 'allow_public_repos turns on, so anyone can queue jobs against a watched public repo.',
            needs: 'root',
            command: 'sudo faber config apply config.toml',
          },
        ],
      },
    }),
  },
};
export const RawRefused1024 = wide(RawRefused);

/** Form changes block the text editor until saved or discarded. */
export const RawBlockedByForm: Story = {
  args: {
    initial: {
      ...raw(),
      draft: edited((c) => {
        (c.autolimit as Config).headroom_mib = 1024;
        (c.autolimit as Config).margin = 1.5;
      }),
    },
  },
};
export const RawBlockedByForm1024 = wide(RawBlockedByForm);

/** 409: the file changed on disk; Reload starts from the new one. */
export const RawStale: Story = { args: { initial: raw({ text: `${RAW_TEXT}# note\n`, outcome: { kind: 'stale' } }) } };
export const RawStale1024 = wide(RawStale);

/** A daemon run with flags has no file to edit. */
export const RawFromFlags: Story = { args: { initial: { ...raw(), view: withView({ from_flags: true }) } } };
export const RawFromFlags1024 = wide(RawFromFlags);
