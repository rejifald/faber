import { Copy, Pause, Play } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { ActionError } from '../dashboard/api';
import { goldenRows, holdRow, hostState, repoHoldRows, runningJobs, updateModel } from '../dashboard/maintenance';
import type { DashboardAction, ViewProps } from '../dashboard/screen';
import type { DaemonState } from '../popover/types';
import { PageHeader } from '../blocks/PageHeader';
import { ControlTile } from '../blocks/ControlTile';
import { Panel } from '../blocks/Panel';
import { Command } from '../blocks/TrustRow';
import { Button, Dialog, Field, IconButton, Input, StatusDot, type StatusTone } from '../kit';
import { plural } from '../popover/format';

export interface MaintenanceProps extends ViewProps {
  /** The last good poll is older than the shell's staleness bound (S10): the live dot turns amber "Stale". */
  stale?: boolean;
}

const reason = (e: unknown) => (e instanceof Error ? e.message : String(e));

interface RowProps {
  /** The fact's name, bold; a state dot goes before it when `tone` is set. */
  title: ReactNode;
  /** The fact. */
  text?: ReactNode;
  tone?: StatusTone;
  /** A quiet line under the fact. */
  note?: ReactNode;
  /** The row's buttons, at the trailing edge. */
  actions?: ReactNode;
  /** Why the last action failed, read out as a status. */
  error?: string;
  /** A command to run in a terminal, with a copy button. */
  command?: string;
}

/** One fact with its action at the trailing edge, and the daemon's own error text under it. */
function Row({ title, text, tone, note, actions, error, command }: RowProps) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex flex-wrap items-start gap-x-lg gap-y-sm border-t border-border-soft px-xl py-md first:border-t-0">
      <div className="flex min-w-0 flex-1 flex-col gap-3xs">
        <span className="flex items-center gap-sm text-small font-semibold text-fg-strong">
          {tone && <StatusDot tone={tone} />}
          {title}
        </span>
        {text && <span className="text-small text-muted wrap-anywhere">{text}</span>}
        {note && <span className="text-caption text-faint wrap-anywhere">{note}</span>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-sm">{actions}</div>}
      <p role="status" className="basis-full text-caption text-red empty:hidden">
        {error}
      </p>
      {command && (
        <div className="flex basis-full items-start gap-sm">
          <div className="min-w-0 flex-1">
            <Command>{command}</Command>
          </div>
          <IconButton
            icon={Copy}
            label={copied ? 'Copied' : 'Copy command'}
            size="sm"
            onClick={() => {
              void navigator.clipboard?.writeText(command).then(
                () => setCopied(true),
                () => {},
              );
            }}
          />
        </div>
      )}
    </div>
  );
}

/** A daemon phrase set as its own line: sentence case, no trailing stop. */
const line = (x: string) => (x[0] ? x[0].toUpperCase() + x.slice(1) : x).replace(/\.$/, '');

/** A healthy default is calm: the tile colours only what needs attention. */
const calm = (t: StatusTone): StatusTone => (t === 'green' ? 'neutral' : t);

/** Sets the version in `text` (the first x.y.z) in mono, the rest as prose. */
const withVersion = (text: string): ReactNode =>
  text.split(/(\d+\.\d+\.\d+\S*)/).map((part, i) =>
    i % 2 ? (
      <span key={i} className="font-mono">
        {part}
      </span>
    ) : (
      part
    ),
  );

/**
 * The Updates tile leads with a state, like every tile: a failure first, then a release on offer,
 * else the phase or "Up to date". An offer is good news, so it is calm (S16): the filled Install
 * now carries it; colour is for a failure (red) or a phase that holds the host (amber: held back,
 * installing). The installed version and the why go underneath,
 * a line each, versions in mono.
 */
function updateTile(u: NonNullable<ReturnType<typeof updateModel>>, phase: string | undefined): { state: string; tone: StatusTone; lines: ReactNode[] } {
  const failed = u.phase?.tone === 'red';
  const state = failed ? u.phase!.word : u.available ? `${u.available.version} available` : (u.phase?.word ?? 'Up to date');
  // Waiting for an idle moment is the routine path to an install, not a hold.
  const tone: StatusTone = u.phase && phase !== 'waiting_idle' ? calm(u.phase.tone) : 'neutral';
  const lines = [
    `Installed ${u.current}`,
    !failed && u.available && u.phase ? u.phase.word : undefined,
    u.phase?.note ?? u.available?.why,
    u.lastCheck && `Last checked ${u.lastCheck}`,
  ];
  return {
    state,
    tone,
    lines: lines.filter((x): x is string => Boolean(x)).map((x) => withVersion(line(x))),
  };
}

/** The Jobs tile's state: what is in flight right now. */
function jobsState(s: DaemonState): string {
  const all = s.inflight || [];
  const parked = all.filter((j) => j.admitted === false).length;
  const running = all.length - parked;
  if (!all.length) return 'No jobs in flight';
  return parked ? `${running} running, ${parked} parked` : `${running} running`;
}

const Empty = ({ children }: { children: string }) => <p className="px-xl py-md text-small text-muted">{children}</p>;

/**
 * The dashboard's Maintenance view (screens.md § 4, S13, S15): every action that changes what this
 * host does. A row of control tiles (Host, Docker admission, Updates, Jobs), each its state with
 * its action; Repository holds and Golden images side by side below; Drain behind a
 * confirmation, last. A failure is the daemon's own text under the fact it belongs to.
 *
 * @useWhen the dashboard's `/maintenance` view. A pure function of the state it is handed;
 * actions go through `run`.
 * @states running, paused, draining, held, repository holds, no golden image, golden failed, update ready, hold dialog open, drain confirm open, action failed
 */
export function Maintenance({ state, now, run, stale = false }: MaintenanceProps) {
  const [pending, setPending] = useState<{ panel: string; id: string }>();
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [sudo, setSudo] = useState<Record<string, boolean>>({});
  const [holdOpen, setHoldOpen] = useState(false);
  const [holdReason, setHoldReason] = useState('');
  const [holdError, setHoldError] = useState<string | undefined>();
  const [drainOpen, setDrainOpen] = useState(false);
  const [dialogBusy, setDialogBusy] = useState(false);
  const [drainError, setDrainError] = useState<string | undefined>();

  /** Runs one row's action: its button spins, the panel's others wait, a failure lands under the row. */
  async function act(panel: string, row: string, label: string, action: DashboardAction) {
    setPending({ panel, id: `${row}:${label}` });
    setErrors((e) => ({ ...e, [row]: '' }));
    setSudo((x) => ({ ...x, [row]: false }));
    try {
      await run(action);
    } catch (e) {
      setErrors((x) => ({ ...x, [row]: `${label} failed: ${reason(e)}` }));
      // The daemon refuses a clear that widens trust (403): the operator runs it with sudo.
      if (action.cmd === 'clear_repo_hold' && e instanceof ActionError && e.status === 403) setSudo((x) => ({ ...x, [row]: true }));
    } finally {
      setPending(undefined);
    }
  }

  /** The button props for one action: loading on its own, disabled while another in the panel runs. */
  const btn = (panel: string, row: string, label: string) => {
    const id = `${row}:${label}`;
    return { loading: pending?.id === id, disabled: pending !== undefined && pending.panel === panel && pending.id !== id };
  };

  async function submitHold() {
    const why = holdReason.trim();
    if (!why) {
      setHoldError('Give a reason, so the next operator knows why Docker is held.');
      return;
    }
    setDialogBusy(true);
    setHoldError(undefined);
    try {
      await run({ cmd: 'hold', reason: why });
      setHoldOpen(false);
      setHoldReason('');
    } catch (e) {
      setHoldError(reason(e));
    } finally {
      setDialogBusy(false);
    }
  }

  async function confirmDrain() {
    setDialogBusy(true);
    setDrainError(undefined);
    try {
      await run({ cmd: 'drain' });
      setDrainOpen(false);
    } catch (e) {
      setDrainError(reason(e));
    } finally {
      setDialogBusy(false);
    }
  }

  const host = hostState(state);
  const hold = holdRow(state, now);
  const repoHolds = repoHoldRows(state, now);
  const golden = goldenRows(state);
  const update = updateModel(state, now);
  const tile = update && updateTile(update, state.update?.phase?.phase);
  const running = runningJobs(state);

  return (
    <div className="flex w-full min-w-0 flex-col gap-2xl">
      <PageHeader title="Maintenance" live={stale ? 'stale' : true} />

      <div className="grid grid-cols-1 gap-lg narrow:grid-cols-2 wide:grid-cols-4">
        <ControlTile
          label="Host"
          state={host.word}
          tone={calm(host.tone)}
          note={host.detail}
          error={errors.host}
        >
          {!state.draining && (
            <Button icon={state.paused ? Play : Pause} {...btn('host', 'host', state.paused ? 'Resume' : 'Pause')} onClick={() => void act('host', 'host', state.paused ? 'Resume' : 'Pause', { cmd: state.paused ? 'resume' : 'pause' })}>
              {state.paused ? 'Resume' : 'Pause'}
            </Button>
          )}
        </ControlTile>

        <ControlTile
          label="Docker admission"
          state={hold.value}
          tone={calm(hold.tone)}
          note={hold.held ? hold.note : 'Docker jobs are admitted as the budget allows.'}
          error={errors.hold}
        >
          {hold.held ? (
            <Button {...btn('hold', 'hold', 'Release')} onClick={() => void act('hold', 'hold', 'Release', { cmd: 'unhold' })}>
              Release
            </Button>
          ) : (
            <Button
              onClick={() => {
                setHoldError(undefined);
                setHoldOpen(true);
              }}
            >
              Hold…
            </Button>
          )}
        </ControlTile>

        {update === null || !tile ? (
          <ControlTile label="Updates" state="Not reported" note="The daemon has not reported on updates yet." />
        ) : (
          <ControlTile
            label="Updates"
            state={tile.state}
            tone={tile.tone}
            note={tile.lines.map((l, i) => <span key={i} className="block">{l}</span>)}
            error={errors['update:offer'] || errors['update:check']}
          >
            {update.canInstall && (
              <Button variant="primary" {...btn('update', 'update:offer', 'Install now')} onClick={() => void act('update', 'update:offer', 'Install now', { cmd: 'apply_update' })}>
                Install now
              </Button>
            )}
            {update.canCheck && (
              <Button {...btn('update', 'update:check', 'Check now')} onClick={() => void act('update', 'update:check', 'Check now', { cmd: 'check_update' })}>
                Check now
              </Button>
            )}
            {update.canSkip && (
              <Button variant="link" {...btn('update', 'update:offer', 'Skip')} onClick={() => void act('update', 'update:offer', 'Skip', { cmd: 'skip_update' })}>
                Skip
              </Button>
            )}
          </ControlTile>
        )}

        <ControlTile
          label="Jobs"
          state={jobsState(state)}
          tone="neutral"
          note="Reconcile asks GitHub for queued jobs now, instead of waiting for the next cycle."
          error={errors.reconcile}
        >
          <Button {...btn('jobs', 'reconcile', 'Reconcile')} onClick={() => void act('jobs', 'reconcile', 'Reconcile', { cmd: 'reconcile' })}>
            Reconcile
          </Button>
        </ControlTile>
      </div>

      <div className={`grid grid-cols-1 items-start gap-lg ${golden.length > 0 ? 'wide:grid-cols-2' : ''}`}>
        <Panel title="Repository holds" meta={repoHolds.length ? plural(repoHolds.length, 'repository', 'repositories') : undefined}>
          {repoHolds.length === 0 ? (
            <Empty>No repository is held.</Empty>
          ) : (
            repoHolds.map((r) => (
              <Row
                key={r.key}
                title={<span className="font-mono">{r.repo}</span>}
                tone="amber"
                text={r.note}
                error={errors[`repo:${r.key}`]}
                command={sudo[`repo:${r.key}`] ? r.command : undefined}
                actions={
                  <Button {...btn('repos', `repo:${r.key}`, 'Clear')} onClick={() => void act('repos', `repo:${r.key}`, 'Clear', { cmd: 'clear_repo_hold', repo: r.repo })}>
                    Clear
                  </Button>
                }
              />
            ))
          )}
        </Panel>

        {golden.length > 0 && (
          <Panel title="Golden images">
            {golden.map((g) => (
              <Row
                key={g.key}
                title={<span className="font-mono">{g.image}</span>}
                tone={g.tone}
                text={g.word}
                note={g.note}
                error={errors[`golden:${g.key}`]}
                actions={
                  <Button {...btn('golden', `golden:${g.key}`, 'Verify now')} onClick={() => void act('golden', `golden:${g.key}`, 'Verify now', { cmd: 'golden_verify', image: g.image })}>
                    Verify now
                  </Button>
                }
              />
            ))}
          </Panel>
        )}
      </div>

      {!state.draining && (
        <div className="flex flex-wrap items-center gap-x-lg gap-y-sm rounded-lg border border-border bg-surface px-xl py-md">
          <p className="min-w-0 flex-1 text-small text-muted wrap-anywhere">
            <span className="font-semibold text-fg-strong">Drain</span>
            {' '}
            Running jobs finish, no new ones start, and faber stays down until it restarts.
          </p>
          <Button
            tone="danger"
            onClick={() => {
              setDrainError(undefined);
              setDrainOpen(true);
            }}
          >
            Drain…
          </Button>
        </div>
      )}

      <Dialog
        open={holdOpen}
        onOpenChange={(o) => {
          if (!dialogBusy) setHoldOpen(o);
        }}
        title="Hold Docker"
        description="No new job starts until the hold is released. Running jobs finish."
        error={holdError}
        actions={
          <>
            <Button variant="link" disabled={dialogBusy} onClick={() => setHoldOpen(false)}>
              Cancel
            </Button>
            <Button variant="primary" loading={dialogBusy} onClick={() => void submitHold()}>
              Hold
            </Button>
          </>
        }
      >
        <Field label="Reason">
          <Input value={holdReason} onValueChange={setHoldReason} placeholder="Resizing the VM" />
        </Field>
      </Dialog>

      <Dialog
        open={drainOpen}
        onOpenChange={(o) => {
          if (!dialogBusy) setDrainOpen(o);
        }}
        title="Drain this host"
        description={`${running ? `${plural(running, 'job')} ${running === 1 ? 'is' : 'are'} running and will finish.` : 'No job is running.'} No new job starts, and faber stays down until it restarts.`}
        error={drainError}
        actions={
          <>
            <Button variant="link" disabled={dialogBusy} onClick={() => setDrainOpen(false)}>
              Cancel
            </Button>
            <Button variant="danger" loading={dialogBusy} onClick={() => void confirmDrain()}>
              Drain
            </Button>
          </>
        }
      >
        <p className="text-small text-muted">To undo it, restart faber. Pause is the reversible stop.</p>
      </Dialog>
    </div>
  );
}
