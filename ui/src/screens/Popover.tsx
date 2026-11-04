import { Ellipsis, ExternalLink, Info, Pause, Play, RotateCw, TriangleAlert } from 'lucide-react';
import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { AttentionRow } from '../blocks/AttentionRow';
import { CompactGroup } from '../blocks/CompactGroup';
import { DismissedList } from '../blocks/DismissedList';
import { FoldToggle } from '../blocks/FoldToggle';
import { GateRow } from '../blocks/GateRow';
import { GaugeBar } from '../blocks/GaugeBar';
import { JobGroup } from '../blocks/JobGroup';
import { PeerRow } from '../blocks/PeerRow';
import { Pipe } from '../blocks/Pipe';
import { PipStrip } from '../blocks/PipStrip';
import { VmSection } from '../blocks/VmSection';
import { LimitsView } from './LimitsView';
import { Button, Icon, IconButton, ScrollArea, Mark, Menu, MenuCheckboxItem, MenuItem, MenuLabel, MenuSeparator } from '../kit';
import {
  MARK,
  appUpdateRows,
  applyAttention,
  canDismiss,
  daemonRows,
  DISMISS,
  RESTORE_ALL,
  dismissAction,
  foldRows,
  restoreAction,
  severityTally,
  trustRows,
  updateRows,
  type Severity,
} from '../popover/attention';
import { baseVersion } from '../popover/format';
import { bindingGate, gateFacts, gaugeModels, type GaugeKey } from '../popover/gauges';
import { PIP_WORD, buildGroups, pipTally, worstFirst } from '../popover/groups';
import { PEERS_MAX, peerRows } from '../popover/peers';
import type { Inflight, Invoke, Snapshot } from '../popover/types';
import { vmSection } from '../popover/vms';
import { CARD_MAX, commands, fitJobs, send, settlePending, startPending, type Command, type Pending } from '../popover/wiring';

/** The fold line's height before it is first drawn: an `sm` control and its gap. */
const MORE_ROW_GUESS = 21;
/** A compact line's height before one is drawn: `small` text, 2 px top and bottom, and its gap (22 + 1 in Storybook). */
const COMPACT_GUESS = 23;
/** The jobs column's own padding, top and bottom (`py-xs`): not room for the groups. */
const JOBS_PADDING = 12;
/** The hairline and gap over the compact lines (`border-t`, `pt-2xs`, and the column's 1 px gap). */
const FOLD_DIVIDER = 6;
/** The severity marks the attention fold's tally draws, and their words. */
const SEVERITY: Record<Severity, { icon: typeof Info; word: string }> = {
  bad: { icon: TriangleAlert, word: 'problem' },
  warn: { icon: TriangleAlert, word: 'warning' },
  info: { icon: Info, word: 'notice' },
};
const GAUGE_PLACEHOLDERS = ['Memory', 'CPU', 'Disk'];

export interface PopoverProps {
  /** The latest snapshot from the app; `null` before the first one arrives (the loading frame). */
  snap: Snapshot | null;
  /** The time the snapshot is read at, in ms: ages and lingering pips count from it. */
  now: number;
  /** Live jobs this app has seen, by id, so a finished run joins its own group. */
  seen?: ReadonlyMap<number, Inflight>;
  /** Runs a Tauri command. */
  invoke: Invoke;
  /** Whether the ⋯ menu is open; the app closes it when the window loses focus. */
  menuOpen?: boolean;
  /** Called when the ⋯ menu opens or closes. */
  onMenuOpenChange?: (open: boolean) => void;
  /** The group open when the screen first renders (a story's open state). */
  defaultOpenGroup?: string | null;
  /** The gate open when the screen first renders (a story's open state). */
  defaultOpenGate?: GaugeKey | null;
  /** The VM row open when the screen first renders (a story's open state), by its key (`linux-0`). */
  defaultOpenVm?: string | null;
  /** Called with the snapshot the app holds when the Limits view's Reconnect re-reads it. */
  onSnapshot?: (snap: Snapshot) => void;
  /** The view shown when the screen first renders (a story's open state). */
  defaultView?: 'main' | 'limits';
  /** Whether the folded job groups show when the screen first renders (a story's open state). */
  defaultJobsOpen?: boolean;
  /** Whether the folded attention rows show when the screen first renders (a story's open state). */
  defaultAttentionOpen?: boolean;
  /** Whether the dismissed rows show when the screen first renders (a story's open state). */
  defaultDismissedOpen?: boolean;
  /** The tallest the card may grow, even to hold an open group: the window's cap by default. */
  maxHeight?: number;
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * The tray popover (popover.md, popover/anatomy.md): does faber need me? The header and its ⋯
 * menu, then every active issue, the gates (bars that open in place), the job groups with their pips, the virtual machines (rows that open in place), other hosts, and
 * Pause or Resume. While job groups show it is 620 px in two columns, gauges and hosts left and
 * the groups right, folded at the gauges' height into compact lines; otherwise a 308 px column.
 * Offline it collapses to the dashboard link; before the first snapshot it shows the loading frame.
 *
 * @useWhen the Tauri popover window, through `ui/popover/main.tsx`, which feeds it snapshots.
 * @states loading, idle, idle with a VM up, idle with a stale peer, an issue and no jobs, running, waiting, finished, busy, busy overflow, folded into compact lines, folded past the compact lines, a folded group open, folded list open, attention list open, a gate open, a gate open while jobs wait, a gate open while idle, a VM open, paused and frozen, attention rows, dismissed rows, dismissed rows open, all dismissed, trust changes acknowledged, mirror down, mirror passing through, mirror image missing, macOS jobs held, action failed, two actions in flight, VMs stale, menu open, limits view, offline
 */
export function Popover({
  snap,
  now,
  seen,
  invoke,
  menuOpen,
  onMenuOpenChange,
  defaultOpenGroup = null,
  defaultOpenGate = null,
  defaultOpenVm = null,
  onSnapshot,
  defaultView = 'main',
  defaultJobsOpen = false,
  defaultAttentionOpen = false,
  defaultDismissedOpen = false,
  maxHeight = CARD_MAX,
}: PopoverProps) {
  const [view, setView] = useState<'main' | 'limits'>(defaultView);
  const [openGate, setOpenGate] = useState<GaugeKey | null>(defaultOpenGate);
  const [openGroup, setOpenGroup] = useState<string | null>(defaultOpenGroup);
  // The two folded lists, open in place (fold.md). Both start shut each time the popover closes.
  const [jobsOpen, setJobsOpen] = useState(false);
  // A story's request to start open waits for a measured fold with something folded: before it, nothing is folded to open.
  const wantJobsOpen = useRef(defaultJobsOpen);
  const [attentionOpen, setAttentionOpen] = useState(defaultAttentionOpen);
  const [dismissedOpen, setDismissedOpen] = useState(defaultDismissedOpen);
  // The caps the open lists scroll at, measured below and handed to the kit's ScrollArea.
  const [jobsMax, setJobsMax] = useState<number>();
  const [attentionMax, setAttentionMax] = useState<number>();
  const [openVm, setOpenVm] = useState<string | null>(defaultOpenVm);
  const [pending, setPending] = useState<Pending>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [autostart, setAutostart] = useState(false);
  // The update wait the app last confirmed, and the snapshot value it overrides: once the
  // snapshot moves on, the snapshot wins again.
  const [floorSet, setFloorSet] = useState<{ secs: number; over: number | null } | null>(null);
  const [ownMenuOpen, setOwnMenuOpen] = useState(false);
  // How the jobs column folds (anatomy § 6): how many groups show in full, then how many get a
  // compact line before "+N more groups". Measured after each render from the gauge column, the
  // card and each drawn group, whose heights are remembered by key.
  const [fold, setFold] = useState({ full: Infinity, compact: Infinity });
  const card = useRef<HTMLElement>(null);
  const columns = useRef<HTMLDivElement>(null);
  const gaugeColumn = useRef<HTMLDivElement>(null);
  const groupBox = useRef<HTMLDivElement>(null);
  const jobsFooter = useRef<HTMLDivElement>(null);
  const attentionList = useRef<HTMLDivElement>(null);
  const jobsId = useId();
  const attentionId = useId();
  const heights = useRef(new Map<string, number>());
  // The compact line's and the more row's last drawn heights. Kept like the groups', since either
  // may not be drawn on the next pass: a guess there that differs from the real height would
  // flip the fold back and forth between two counts.
  const rowHeights = useRef<{ compact?: number; more?: number }>({});
  const isMenuOpen = menuOpen ?? ownMenuOpen;

  const fail = (place: string, e: unknown, hint?: string) =>
    setErrors((all) => ({ ...all, [place]: [errorText(e), hint].filter(Boolean).join(' ') }));

  /** Runs `c` as `label` in `place`: loading there until it settles, and its error beside it. */
  const run = (place: string, label: string, c: Command, failHint?: string) => {
    setPending((p) => startPending(p, place, label));
    setErrors((all) => {
      const rest = { ...all };
      delete rest[place];
      return rest;
    });
    send(invoke, c)
      .catch((e: unknown) => fail(place, e, failHint))
      .finally(() => setPending((p) => settlePending(p, place, label)));
  };
  /** The other actions of `place` wait while one runs; navigation never does. */
  const busyIn = (place: string) => pending[place];

  const clearErrors = () => setErrors((all) => (Object.keys(all).length ? {} : all));

  const setMenu = (open: boolean) => {
    setOwnMenuOpen(open);
    onMenuOpenChange?.(open);
  };

  // Each time the menu opens, re-read Start at login. It is painted from what the backend
  // reports, never from what was asked, so a login item that refuses to register shows as off.
  useEffect(() => {
    if (!isMenuOpen) return;
    let live = true;
    send(invoke, commands.readAutostart()).then(
      (on) => live && setAutostart(Boolean(on)),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [isMenuOpen, invoke]);

  // The popover hides on blur: both lists are shut when it next shows.
  useEffect(() => {
    const shut = () => {
      setJobsOpen(false);
      setAttentionOpen(false);
      setDismissedOpen(false);
    };
    window.addEventListener('blur', shut);
    return () => window.removeEventListener('blur', shut);
  }, []);

  const online = Boolean(snap?.online && snap.state);
  const s = (online && snap?.state) || {};
  const u = s.update || {};

  let version = '';
  if (online && u.current) {
    version = `faber v${u.current}`;
  } else if (!online && snap?.app_version) {
    version = `app v${snap.app_version}`;
  }
  const drift = online && u.current && snap?.app_version && baseVersion(snap.app_version) !== baseVersion(u.current);
  const snapFloor = snap?.app_update?.floor_secs ?? null;
  // Forgotten, not just outranked, once the snapshot moves: a snapshot that later returns to the
  // old value is the app's word, not a reason to show the stale override again.
  if (floorSet && floorSet.over !== snapFloor) setFloorSet(null);
  const floor = floorSet && floorSet.over === snapFloor ? floorSet.secs : snapFloor;
  const floorLabel = floor ? `App update wait: ${floor % 3600 === 0 ? `${floor / 3600} h` : `${Math.round(floor / 60)} min`}` : 'App update wait';

  const header = (
    <header className="flex items-center gap-sm py-md pr-lg pl-xl">
      <Mark variant="logo" size="lg" />
      <span className="text-title font-semibold text-fg-strong">faber</span>
      <span className="ml-auto">
        <Menu label="More" icon={Ellipsis} iconOnly open={isMenuOpen} onOpenChange={setMenu}>
          {version && (
            <MenuLabel>
              {version}
              {drift && (
                <>
                  <Pipe />
                  {`app v${snap?.app_version}`}
                </>
              )}
            </MenuLabel>
          )}
          {version && <MenuSeparator />}
          <MenuItem onClick={() => setView('limits')}>Limits…</MenuItem>
          <MenuItem shortcut="⌘," onClick={() => void send(invoke, commands.openSettings()).catch(() => {})}>
            Settings…
          </MenuItem>
          <MenuItem shortcut="⌘⇧F" onClick={() => void send(invoke, commands.showPanel()).catch(() => {})}>
            Open panel
          </MenuItem>
          <MenuItem onClick={() => void send(invoke, commands.openDashboard()).catch(() => {})}>Open dashboard</MenuItem>
          <MenuCheckboxItem
            checked={autostart}
            onCheckedChange={(on) =>
              send(invoke, commands.setAutostart(on)).then(
                (now) => setAutostart(Boolean(now)),
                (e: unknown) => fail('menu', e),
              )
            }
          >
            Start at login
          </MenuCheckboxItem>
          <MenuItem
            keepOpen
            onClick={() =>
              // The label shows what the app reports it saved, never what was asked.
              send(invoke, commands.setFloor(floor)).then(
                (secs) => setFloorSet({ secs: Number(secs), over: snapFloor }),
                (e: unknown) => {
                  setFloorSet(null);
                  fail('menu', e);
                },
              )
            }
          >
            {floorLabel}
          </MenuItem>
          {errors.menu && <MenuLabel tone="error">{errors.menu}</MenuLabel>}
          <MenuSeparator />
          <MenuItem shortcut="⌘Q" onClick={() => void send(invoke, commands.quit()).catch(() => {})}>
            Quit faber
          </MenuItem>
        </Menu>
      </span>
    </header>
  );

  let body: ReactNode;
  let groupKeys: string[] = [];
  let openIndex = -1;
  // Two columns while any job group shows (popover.md § Two columns); one otherwise.
  let wide = false;
  if (view === 'limits') {
    body = (
      <LimitsView
        snap={snap}
        invoke={invoke}
        onBack={() => setView('main')}
        onOpenSettings={() => void send(invoke, commands.openSettings()).catch(() => {})}
        onReconnect={() => send(invoke, commands.snapshot()).then((fresh) => onSnapshot?.(fresh as Snapshot))}
      />
    );
  } else if (!snap) {
    body = <Loading />;
  } else if (!online) {
    body = (
      <div className="flex flex-col items-center gap-2xs px-2xl pt-xs pb-2xl text-center">
        <span className="mb-sm flex rounded-full bg-surface-2 p-lg text-muted">
          <Mark variant="offline" size="lg" />
        </span>
        <p className="text-title font-semibold text-fg-strong">Daemon offline</p>
        <p className="text-body text-muted">faber isn’t reachable right now.</p>
        <p className="mb-lg font-mono text-caption wrap-break-word text-faint">
          {[snap.base_url, snap.error].filter(Boolean).join(' — ')}
        </p>
        <Button size="lg" variant="primary" icon={ExternalLink} onClick={() => void send(invoke, commands.openDashboard()).catch(() => {})}>
          Open dashboard in browser
        </Button>
      </div>
    );
  } else {
    // ADR-0064: acknowledged trust changes fold into one row, and dismissed rows leave the list
    // (and its tally) for the "N dismissed" line under it.
    const { active: attention, dismissed } = applyAttention(
      [...daemonRows(s, now), ...updateRows(s.update), ...appUpdateRows(snap.app_update), ...trustRows(s, snap.trust_shrank, now)],
      s.attention,
    );
    const { shown, hidden } = foldRows(attention);
    const live = new Set(attention.map((r) => `row:${r.key}`));
    const { gate, parked } = bindingGate(s);
    const gauges = gaugeModels(s.budget || {}, gate, parked);
    const groups = buildGroups(s, seen ?? new Map(), now);
    const idle = s.healthy !== false && attention.length === 0 && groups.length === 0;
    const peers = peerRows(s.peers, now).filter((p) => !idle || p.stale);
    // Shown, not attention: VMs never make the popover non-idle, and show while idle (vms/anatomy.md § 6).
    const vms = vmSection(s.budget, now);
    const paused = Boolean(s.paused);
    // An open group whose jobs are gone closes, rather than reopening if its key comes back.
    if (openGroup && !groups.some((g) => g.key === openGroup)) setOpenGroup(null);
    groupKeys = groups.map((g) => g.key);
    openIndex = openGroup ? groupKeys.indexOf(openGroup) : -1;
    wide = groups.length > 0;
    const showPanel = () => void send(invoke, commands.showPanel()).catch(() => {});
    const full = groups.slice(0, fold.full);
    // Past the groups shown in full, the worst go first, so a runner failure is never only a count.
    const folded = worstFirst(groups.slice(full.length));
    const compact = folded.slice(0, fold.compact);
    const rest = folded.length - compact.length;
    // Nothing folded: nothing is open.
    if (jobsOpen && folded.length === 0) setJobsOpen(false);
    if (attentionOpen && hidden === 0) setAttentionOpen(false);
    if (dismissedOpen && dismissed.length === 0) setDismissedOpen(false);
    const foldedRows = attention.slice(shown.length);
    const hasAttention = shown.length > 0 || dismissed.length > 0;

    // An open gate whose gate is gone (the ceiling unset, the CPU budget dropped) closes.
    if (openGate && !gauges.some((g) => g.key === openGate)) setOpenGate(null);
    const gates = gauges.length > 0 && (
      // Each gate opens in place to say what its bar is made of (popover.md § Hero).
      <div className="flex flex-col gap-px py-xs">
        {gauges.map((g) => (
          <GateRow
            key={g.key}
            gauge={g}
            facts={gateFacts(g, s.budget || {})}
            open={openGate === g.key}
            onOpenChange={(open) => {
              // One open row in the left column: a gate closes an open VM (owner, 2026-10-31).
              setOpenGate(open ? g.key : null);
              if (open) setOpenVm(null);
            }}
            onAdjust={() => setView('limits')}
          />
        ))}
      </div>
    );

    // An open VM row whose VM is gone (stopped) closes.
    if (openVm && !vms.rows.some((r) => r.key === openVm)) setOpenVm(null);
    const machines = vms.rows.length > 0 && (
      <>
        <hr className="mx-xl border-border" />
        <VmSection
          {...vms}
          openKey={openVm}
          onOpenChange={(key) => {
            setOpenVm(key);
            if (key) setOpenGate(null);
          }}
        />
      </>
    );
    const hosts = peers.length > 0 && (
      <>
        <hr className="mx-xl border-border" />
        <div className="flex flex-col py-xs">
          {/* Labelled whenever VM rows sit above, so a peer is never read as one (vms/anatomy.md § 6). */}
          {(!idle || vms.rows.length > 0) && (
            <p className="px-xl pt-xs pb-2xs text-caption font-semibold text-faint">Other hosts</p>
          )}
          {peers.slice(0, PEERS_MAX).map((p) => (
            <PeerRow key={p.key} peer={p} />
          ))}
          {peers.length > PEERS_MAX && <p className="px-xl text-caption text-faint">{`+${peers.length - PEERS_MAX} more hosts`}</p>}
        </div>
      </>
    );

    body = (
      <>
        {hasAttention && (
          <div className="flex flex-col divide-y divide-bg border-b border-border">
            {/* Scrolls only when the open list would take the card past the window's cap (see the layout effect). */}
            {shown.length > 0 && (
              <ScrollArea ref={attentionList} id={attentionId} maxHeight={attentionOpen ? attentionMax : undefined}>
                <div className="flex flex-col divide-y divide-bg">
                {(attentionOpen ? attention : shown).map((r) => (
                  <AttentionRow
                    key={r.key}
                    row={r}
                    pending={busyIn(`row:${r.key}`)}
                    error={live.has(`row:${r.key}`) ? errors[`row:${r.key}`] : undefined}
                    onAction={(a) => run(`row:${r.key}`, a.label, commands.row(a), a.failHint)}
                    onDismiss={canDismiss(r) ? () => run(`row:${r.key}`, DISMISS, commands.row(dismissAction(r))) : undefined}
                  />
                ))}
                </div>
              </ScrollArea>
            )}
            {hidden > 0 && (
              <FoldToggle
                count={hidden}
                open={attentionOpen}
                onOpenChange={setAttentionOpen}
                controls={attentionId}
                tally={severityTally(foldedRows).map(({ sev, count }) => ({
                  key: sev,
                  count,
                  label: SEVERITY[sev].word,
                  mark: (
                    <span className={`flex ${MARK[sev]}`}>
                      <Icon icon={SEVERITY[sev].icon} size="sm" />
                    </span>
                  ),
                }))}
              />
            )}
            {dismissed.length > 0 && (
              <DismissedList
                rows={dismissed}
                open={dismissedOpen}
                onOpenChange={setDismissedOpen}
                pending={busyIn('dismissed')}
                error={errors.dismissed}
                onRestore={(key) => run('dismissed', key ?? RESTORE_ALL, commands.row(restoreAction(key)))}
              />
            )}
          </div>
        )}
        {wide ? (
          <div ref={columns} data-columns className={`flex ${hasAttention ? '' : 'border-t border-border'}`}>
            <div className="w-gauge-col shrink-0 border-r border-border">
              <div ref={gaugeColumn} data-gauges>
                {gates}
                {machines}
                {hosts}
              </div>
            </div>
            {/* Its own height, not the row's: the fold reads what the groups take. */}
            <div ref={groupBox} className="flex min-w-0 flex-1 flex-col gap-px self-start py-xs">
              {/* Open, the column scrolls at the height it folded at (measured below): it never grows. */}
              <ScrollArea id={jobsId} maxHeight={jobsOpen ? jobsMax : undefined} fades={jobsOpen}>
                <div className="flex flex-col gap-px">
                  {full.map((g) => (
                    <div key={g.key} data-group={g.key}>
                      <JobGroup group={g} open={openGroup === g.key} onOpenChange={(open) => setOpenGroup(open ? g.key : null)} />
                    </div>
                  ))}
                  {folded.length > 0 && (
                    <div data-folded className="flex flex-col gap-px border-t border-border pt-2xs">
                      {(jobsOpen ? folded : compact).map((g) => (
                        <div key={g.key} data-compact>
                          <CompactGroup group={g} />
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </ScrollArea>
              {/* One slot for the toggle in both states, so focus stays on it and its chevron turns when it flips. */}
              {folded.length > 0 && (jobsOpen || rest > 0) && (
                <div ref={jobsFooter} {...(jobsOpen ? {} : { 'data-more': '' })}>
                  <FoldToggle
                    count={jobsOpen ? folded.length : rest}
                    open={jobsOpen}
                    onOpenChange={setJobsOpen}
                    onPanel={showPanel}
                    controls={jobsId}
                    tally={pipTally(folded.slice(compact.length)).map(({ kind, count }) => ({
                      key: kind,
                      count,
                      label: PIP_WORD[kind],
                      mark: <PipStrip pips={[{ kind, key: kind }]} still />,
                    }))}
                  />
                </div>
              )}
            </div>
          </div>
        ) : (
          <>
            {gates}
            {machines}
            {hosts}
          </>
        )}
        <div className="flex items-center justify-between border-t border-border px-sm py-xs">
          <Button
            size="lg"
            variant={paused ? 'primary' : 'ghost'}
            icon={paused ? Play : Pause}
            loading={busyIn('actions') === 'pause'}
            disabled={busyIn('actions') === 'reconcile'}
            onClick={() => run('actions', 'pause', commands.pause(paused))}
          >
            {paused ? 'Resume' : 'Pause'}
          </Button>
          <span role="status" className="min-w-0 flex-1 truncate px-xs text-caption text-red" title={errors.actions}>
            {errors.actions}
          </span>
          {/* A healthy idle popover is header, gauges and Pause (popover.md § Healthy idle). */}
          {!idle && (
            <IconButton
              size="lg"
              icon={RotateCw}
              label="Reconcile now"
              loading={busyIn('actions') === 'reconcile'}
              disabled={busyIn('actions') === 'pause'}
              onClick={() => run('actions', 'reconcile', commands.reconcile())}
            />
          )}
        </div>
      </>
    );
  }

  // Fold the jobs column (anatomy § 6, owner 2026-10-26): at the gauge column's height, so jobs
  // never make the card taller, and never past the window's cap. Runs after every render, since
  // any snapshot can change a group's height; it sets state only when the fold changes.
  // eslint-disable-next-line react-hooks/exhaustive-deps -- no deps on purpose: it measures after every render, as above
  useLayoutEffect(() => {
    const box = groupBox.current;
    const column = gaugeColumn.current;
    const row = columns.current;
    const el = card.current;
    if (!box || !column || !row || !el) return;
    // Open, the jobs column scrolls at its folded height: the gauge column's, less its padding and
    // the footer. The fold is not re-measured then, so it stays where it was when the list opened.
    if (jobsOpen) {
      const cap = Math.max(0, column.offsetHeight - JOBS_PADDING - (jobsFooter.current?.offsetHeight ?? 0));
      if (cap !== jobsMax) setJobsMax(cap);
      return;
    }
    for (const n of Array.from(box.querySelectorAll<HTMLElement>('[data-group]'))) {
      // Each group's height plus the 1 px gap before the next.
      heights.current.set(n.dataset.group ?? '', n.offsetHeight + 1);
    }
    const measured = (which: 'compact' | 'more', selector: string, guess: number) => {
      const n = box.querySelector<HTMLElement>(selector);
      if (n) rowHeights.current[which] = n.offsetHeight + 1;
      return rowHeights.current[which] ?? guess;
    };
    const next = fitJobs({
      heights: groupKeys.map((k) => heights.current.get(k)),
      room: column.offsetHeight - JOBS_PADDING,
      cap: maxHeight - (el.offsetHeight - row.offsetHeight) - JOBS_PADDING,
      openIndex,
      compact: measured('compact', '[data-compact]', COMPACT_GUESS),
      more: measured('more', '[data-more]', MORE_ROW_GUESS),
      divider: FOLD_DIVIDER,
    });
    if (next.full !== fold.full || next.compact !== fold.compact) setFold(next);
    if (wantJobsOpen.current && next.full < groupKeys.length) {
      wantJobsOpen.current = false;
      setJobsOpen(true);
    }
  });

  // The open attention list grows the card; past the window's cap its rows scroll instead.
  // eslint-disable-next-line react-hooks/exhaustive-deps -- no deps on purpose: it measures after every render, and sets state only when the cap changes
  useLayoutEffect(() => {
    const list = attentionList.current;
    const el = card.current;
    if (!list || !el) return;
    // The card without the list, and the list's own content height (whatever cap it wears now).
    const room = maxHeight - (el.offsetHeight - list.offsetHeight);
    const cap = attentionOpen && list.scrollHeight > room ? Math.max(0, room) : undefined;
    if (cap !== attentionMax) setAttentionMax(cap);
  });

  return (
    <div className="pt-lg">
      <section
        ref={card}
        aria-label="faber"
        // A failure note clears on the next interaction, wherever it lands (guidelines § Controls).
        onPointerDownCapture={clearErrors}
        onKeyDownCapture={clearErrors}
        onKeyDown={(e) => {
          // Escape leaves the Limits view; with the ⋯ menu open it closes the menu first.
          if (e.key === 'Escape' && view === 'limits' && !isMenuOpen) setView('main');
        }}
        className={`relative mx-auto ${wide ? 'w-popover-wide' : 'w-popover'} rounded-lg border border-border bg-bg font-sans text-body text-fg shadow-floating`}>
        <Beak />
        {/* The Limits view replaces the whole card, header included, as drawn (settings/anatomy.md § 6). */}
        {view === 'main' && header}
        {body}
      </section>
    </div>
  );
}

/** The card's pointer up at the menu-bar icon. Its centre is 40 px in from the card's right edge: POPOVER_BEAK_FROM_LEFT in lib.rs. */
function Beak() {
  return (
    <svg width={16} height={9} viewBox="0 0 16 9" aria-hidden className="absolute right-4xl bottom-full translate-y-px">
      <path d="M0 9 L8 1 L16 9 Z" className="fill-bg" />
      <path d="M0 9 L8 1 L16 9" strokeWidth={1} className="fill-none stroke-border" />
    </svg>
  );
}

/** The first frame, before any snapshot: empty gauge tracks and two bars where the jobs go. */
function Loading() {
  return (
    <div aria-busy className="flex flex-col pb-xl">
      <div className="flex flex-col px-xl pt-xs pb-lg">
        {GAUGE_PLACEHOLDERS.map((p) => (
          <div key={p} className="py-xs">
            <GaugeBar placeholder={p} />
          </div>
        ))}
      </div>
      <hr className="mx-xl border-border" />
      <div className="flex flex-col gap-sm px-xl pt-lg">
        <span className="h-lg w-4/5 rounded-full bg-surface-2" />
        <span className="h-lg w-3/5 rounded-full bg-surface-2" />
      </div>
    </div>
  );
}

export interface PopoverFallbackProps {
  /** Runs a Tauri command. */
  invoke: Invoke;
}

/**
 * What the popover shows when a snapshot it cannot draw arrives: the header, one line saying so,
 * and the two ways out, the panel and Quit. The next snapshot tries the screen again.
 *
 * @useWhen only as the popover entry's error fallback (`PopoverApp`).
 * @states default
 */
export function PopoverFallback({ invoke }: PopoverFallbackProps) {
  return (
    <div className="pt-lg">
      <section aria-label="faber" className="mx-auto w-popover rounded-lg border border-border bg-bg font-sans text-body text-fg shadow-floating">
        <header className="flex items-center gap-sm py-md pr-lg pl-xl">
          <Mark variant="logo" size="lg" />
          <span className="text-title font-semibold text-fg-strong">faber</span>
        </header>
        <div className="flex flex-col gap-md px-xl pb-xl">
          <p role="status" className="text-body text-muted">
            The popover could not draw the daemon’s latest state. It tries again with the next one.
          </p>
          <div className="flex items-center gap-sm">
            <Button size="lg" variant="primary" onClick={() => void send(invoke, commands.showPanel()).catch(() => {})}>
              Open panel
            </Button>
            <Button size="lg" variant="link" onClick={() => void send(invoke, commands.quit()).catch(() => {})}>
              Quit faber
            </Button>
          </div>
        </div>
      </section>
    </div>
  );
}
