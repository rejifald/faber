import type { FieldSpec, SectionSpec, SettingsView } from './types';

// A realistic SettingsView for stories: the schema's sections and fields as src/settings.rs sends
// them (a representative subset), with owner/demo repos and the profiles of the reference images.

const f = (key: string, label: string, kind: FieldSpec['kind'], extra: Partial<FieldSpec> = {}): FieldSpec => ({
  key,
  label,
  kind,
  live: false,
  optional: false,
  ...extra,
});

export const LOGIN_ITEM = { pattern: '^[A-Za-z0-9_]+(-[A-Za-z0-9_]+)*(\\[bot\\])?$', example: 'someone-else', error: 'Not a GitHub login: letters, digits, underscores and single hyphens only.' };
export const REPO_ITEM = { pattern: '[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', example: 'owner/repo', error: 'Write it as owner/repo.' };
export const GRANT_ITEM = {
  pattern: '[0-9.]+/[0-9]+ [0-9]+/(?:tcp|udp)',
  example: '192.0.2.5/32 5432/tcp',
  error: 'Write it as ADDRESS/PREFIX PORT/PROTO, for example 192.0.2.5/32 5432/tcp.',
};

// The config key `margin` as a constant: the style-object lint reads a bare `margin:` as CSS.
const MARGIN = 'margin';

export const fields = {
  topic: f('topic', 'Topic', 'text', { hint: 'The GitHub repo topic that opts a repo into faber.' }),
  poll: f('poll_interval_secs', 'Poll interval', 'int', { unit: 's', min: 1 }),
  timeout: f('runner_timeout_secs', 'Runner timeout', 'int', { unit: 's', min: 0, optional: true, hint: 'Lifetime cap on a runner container. Blank = 3h; 0 = uncapped.' }),
  provisioning: f('provisioning', 'Provisioning', 'enum', { options: ['per-job', 'demand'] }),
  events: f('gate.allowed_events', 'Events', 'lines', {
    sudo: 'sudo faber config set gate.allowed_events <events>',
    narrows: 'remove',
    hint: 'Trigger events faber provisions for. Empty refuses everything.',
  }),
  publicRepos: f('gate.allow_public_repos', 'Service public repos', 'bool', {
    sudo: 'sudo faber config set gate.allow_public_repos true',
    narrow: { label: 'Turn off', value: 'false' },
    hint: 'Off keeps faber to private repos.',
  }),
  onBypass: f('gate.on_bypass', 'On a gate bypass', 'enum', {
    sudo: 'sudo faber config set gate.on_bypass record',
    narrow: { label: 'Enforce', value: '"enforce"' },
    options: ['record', 'enforce'],
    hint: 'What happens when a refused job runs anyway.',
  }),
  trusted: f('trusted_actors', 'Trusted actors', 'lines', {
    sudo: 'sudo faber config set trusted_actors <logins>',
    narrows: 'remove-keep-one',
    hint: 'Empty trusts by provenance alone.',
  }),
  denied: f('denied_actors', 'Denied actors', 'lines', {
    sudo: 'sudo faber config set denied_actors <logins>',
    narrows: 'add',
    item: LOGIN_ITEM,
    hint: 'Never provisioned for, whatever the provenance.',
  }),
  labels: f('labels', 'Labels', 'csv', { sudo: 'sudo faber config set profile.<profile>.labels <labels>', hint: "The job's runs-on set, comma-separated. Lead with `faber`." }),
  reserve: f('autolimit.reserve_mib', 'Reserve', 'int', { unit: 'MiB', min: 0, live: true, hint: 'MiB kept for the VM itself' }),
  headroom: f('autolimit.headroom_mib', 'Headroom', 'int', { unit: 'MiB', min: 1, live: true, hint: 'MiB left free between jobs' }),
  margin: f('autolimit.margin', 'Margin', 'float', { unit: '×', min: 1, step: 0.05, live: true }),
  native: f('autolimit.native_concurrency', 'Native concurrency', 'int', { min: 1, live: true }),
  ceiling: f('autolimit.hard_ceiling', 'Hard ceiling', 'ceiling', { min: 1, live: true, hint: 'Most jobs at once' }),
  reservation: f('autolimit.reserve', 'Reservation', 'enum', { options: ['p75', 'max'], optional: true }),
  priority: f('autolimit.priority_repos', 'Priority repos', 'lines', { optional: true, item: REPO_ITEM, hint: 'Admitted ahead of the rest. Empty is first come, first served.' }),
  cpu: f('autolimit.cpu', 'CPU admission', 'enum', { options: ['auto', 'off'], optional: true }),
  overcommit: f('autolimit.cpu_overcommit', 'CPU overcommit', 'float', { unit: '×', min: 0.1, step: 0.05 }),
  reviveDocker: f('host.revive_docker', 'Revive Docker', 'bool', { hint: 'Restart a wedged Docker daemon instead of only refusing to use it.' }),
  diskFloor: f('host.disk_floor_gib', 'Disk floor', 'int', { unit: 'GiB', min: 0 }),
  pullUnqualified: f('host.pull_unqualified_images', 'Pull unqualified images', 'bool', { optional: true, unstated_saves: 1, options: ['not stated, pulls them, warned', 'yes', 'no'] }),
  updatesEnabled: f('update.enabled', 'Enabled', 'bool', { live: true, sudo: 'sudo faber config set update.enabled true', narrow: { label: 'Turn off', value: 'false' } }),
  driftMode: f('playwright_drift.mode', 'Mode', 'enum', { options: ['off', 'warn'] }),
  imageBuild: f('runner_image.build', 'Build', 'enum', { options: ['off', 'on-demand'] }),
  imageKeep: f('runner_image.keep', 'Keep warm', 'int', { min: 0, hint: 'Variants kept built while they are hot.' }),
  imageTtl: f('runner_image.idle_ttl_hours', 'Idle TTL', 'int', { unit: 'h', min: 1 }),
  retryEnabled: f('retry.enabled', 'Enabled', 'bool'),
  retryAttempts: f('retry.max_attempts', 'Max attempts', 'int', { min: 1 }),
  retryLookback: f('retry.lookback_secs', 'Lookback', 'int', { unit: 's', min: 60 }),
  vmCpus: f('linux_vm.cpus', 'Linux VM vCPUs', 'int', { optional: true, min: 1, hint: 'Blank = 4' }),
  vmMemory: f('linux_vm.memory_mib', 'Linux VM memory', 'int', { optional: true, unit: 'MiB', min: 1024, hint: 'Blank = 8192' }),
  registryEnabled: f('registry_auth.enabled', 'Enabled', 'bool', { held: true, narrow: { label: 'Turn off', value: 'false' }, hint: "Held: a credential mounted into a job can be read by the job's code." }),
  registryServer: f('registry_auth.server', 'Server', 'text', { held: true, hint: 'The registry as keyed in a docker config.json. Held with the section.' }),
  registryUser: f('registry_auth.username', 'Username', 'text', { held: true, optional: true, hint: 'Held with the section.' }),
  registryPassword: f('registry_auth.password_env', 'Password variable', 'text', { held: true, hint: 'Held with the section.' }),
  checkInterval: f('update.poll_interval_secs', 'Check interval', 'int', { unit: 's', min: 60, live: true }),
};

const sections: SectionSpec[] = [
  { title: 'Daemon', hint: 'Discovery and the reconcile loop.', fields: [fields.topic, fields.poll, fields.timeout, fields.provisioning] },
  {
    title: 'Trust',
    hint: 'The boundary this host enforces. You can narrow it here. Widening it needs sudo, in a terminal. Applies on restart.',
    fields: [fields.events, fields.publicRepos, fields.onBypass, fields.trusted, fields.denied],
  },
  {
    title: 'Autolimiter',
    hint: "How much of the engine faber admits jobs into. Changes apply to the running daemon at once.",
    fields: [fields.reserve, fields.headroom, fields.margin, fields.native, fields.ceiling, fields.reservation, fields.priority, fields.cpu, fields.overcommit],
  },
  { title: 'Runner-image drift', hint: 'A watched repo may narrow this with its own .faber/config.toml.', fields: [fields.driftMode] },
  { title: 'Runner images', hint: 'Off by default: with it off the image is a static property of the profile.', fields: [fields.imageBuild, fields.imageKeep, fields.imageTtl] },
  { title: 'Auto-retry', hint: 'Off by default: enabling it grants faber the outward-facing action of re-running a workflow.', fields: [fields.retryEnabled, fields.retryAttempts, fields.retryLookback] },
  { title: 'Docker host', hint: "faber is the Docker manager on this box.", fields: [fields.reviveDocker, fields.diskFloor, fields.pullUnqualified] },
  { title: 'Linux VM', hint: 'On a set-up macOS host faber’s Docker runs in a Linux VM of its own. Applies on restart.', fields: [fields.vmCpus, fields.vmMemory] },
  { title: 'Updates', hint: 'Off by default.', fields: [fields.updatesEnabled, fields.checkInterval] },
  { title: 'Registry auth', hint: "Held. Credentials for the images a job pulls would be readable by the job's code, so faber mounts none.", fields: [fields.registryEnabled, fields.registryServer, fields.registryUser, fields.registryPassword] },
];

export const profileFields: FieldSpec[] = [
  f('name', 'Name', 'text'),
  fields.labels,
  f('backend', 'Backend', 'enum', { options: ['docker', 'macos-vm'] }),
  f('image', 'Runner image', 'text', { sudo: 'sudo faber config set profile.<profile>.image <image>', optional: true }),
  f('cpus', 'VM vCPUs', 'int', { optional: true, hint: 'Blank = 4' }),
  f('memory_mib', 'VM memory', 'int', { optional: true, hint: 'MiB per VM. Blank = 8192' }),
  f('dind', 'Docker access', 'bool', {
    sudo: 'sudo faber config set profile.<profile>.dind true',
    narrow: { label: 'Revoke', value: 'false' },
    optional: true,
    options: ['not stated, grants it', 'yes', 'no'],
  }),
  f('cache_volume', 'Cache volume', 'bool', { sudo: 'sudo faber config set profile.<profile>.cache_volume true', narrow: { label: 'Turn off', value: 'false' } }),
  f('git_mirror', 'Git mirror', 'bool'),
  f('mirror', 'Pull-through mirror', 'bool'),
  f('drop_grants', 'Dropped grants', 'csv', {
    sudo: 'sudo faber config set profile.<profile>.drop_grants <grants>',
    narrows: 'add',
    item: GRANT_ITEM,
    optional: true,
    hint: "Grants this profile's jobs must not have, as ADDRESS/PREFIX PORT/PROTO. Adding one narrows.",
  }),
  f('env', 'Environment', 'map', {
    sudo: 'sudo faber config apply <edited config.toml>',
    narrows: 'remove',
    narrow: { label: 'Clear', value: '{}' },
    hint: 'Passed to the runner ahead of every step. Removing an entry narrows; adding or changing one needs sudo.',
  }),
];

export const profiles = [
  {
    name: 'linux-docker',
    labels: ['faber', 'docker'],
    backend: 'docker',
    image: 'ghcr.io/owner/demo-runner:2.329.0',
    dind: true,
    cache_volume: true,
    git_mirror: true,
    mirror: false,
    drop_grants: ['192.0.2.5/32 5432/tcp', '192.0.2.9/32 6379/tcp'],
    env: { HTTPS_PROXY: 'http://198.51.100.2:3128', NO_PROXY: 'localhost,127.0.0.1,192.0.2.0/24', CI_DEBUG: '1' },
  },
  { name: 'linux', labels: ['faber', 'linux'], backend: 'docker', image: 'ghcr.io/owner/demo-runner:2.329.0', dind: false },
  { name: 'macos', labels: ['faber', 'macos'], backend: 'macos-vm', image: 'macos-golden', cpus: 4, memory_mib: 8192 },
];

/** The view a loaded daemon sends, for stories. */
export const settingsView: SettingsView = {
  path: '/Library/Application Support/faber/config.toml',
  exists: true,
  from_flags: false,
  port: 7878,
  config: {
    topic: 'faber',
    poll_interval_secs: 30,
    provisioning: 'demand',
    gate: { allowed_events: ['push', 'pull_request', 'schedule', 'workflow_dispatch'], allow_public_repos: true, on_bypass: 'record' },
    trusted_actors: ['owner', 'release-bot'],
    denied_actors: ['someone-else'],
    autolimit: { reserve_mib: 2048, headroom_mib: 1536, [MARGIN]: 1.2, native_concurrency: 2, hard_ceiling: 4, priority_repos: ['owner/demo', 'owner/docs'], cpu: 'auto', cpu_overcommit: 2 },
    host: { revive_docker: true, disk_floor_gib: 10 },
    update: { enabled: false, poll_interval_secs: 3600 },
    playwright_drift: { mode: 'warn' },
    runner_image: { build: 'on-demand', keep: 2, idle_ttl_hours: 24 },
    retry: { enabled: false, max_attempts: 2, lookback_secs: 3600 },
    linux_vm: {},
    registry_auth: { enabled: false, server: 'ghcr.io' },
    profile: profiles,
  },
  defaults: { poll_interval_secs: 30, autolimit: { [MARGIN]: 1.2 }, host: { disk_floor_gib: 10 } },
  pending_restart: [],
  schema: { sections, profile_fields: profileFields },
};
