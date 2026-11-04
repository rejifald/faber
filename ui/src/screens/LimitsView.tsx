import { useEffect, useReducer, useRef, useState, type ReactNode } from 'react';
import { LimitRow } from '../blocks/LimitRow';
import { MemoryBar } from '../blocks/MemoryBar';
import { ViewHeader } from '../blocks/ViewHeader';
import { Button } from '../kit';
import {
  APPLIED_MS,
  IDLE_ROW,
  KNOBS,
  ceilingView,
  debouncer,
  knobValue,
  limitsAvailable,
  memorySegments,
  rowReducer,
  showKnob,
  type ConfigArg,
  type KnobKey,
  type KnobSpec,
  type RowAction,
  type RowState,
} from '../popover/limits';
import type { Invoke, Snapshot } from '../popover/types';
import { commands, send } from '../popover/wiring';

type Rows = Partial<Record<KnobKey, RowState>>;

export interface LimitsViewProps {
  /** The latest snapshot; `null` before the first one (the loading frame). */
  snap: Snapshot | null;
  /** Runs a Tauri command. */
  invoke: Invoke;
  /** Back to the main view. */
  onBack: () => void;
  /** Opens All settings: the dashboard's /settings page, through the app. */
  onOpenSettings: () => void;
  /** Re-reads the snapshot; the unavailable state's Reconnect. */
  onReconnect?: () => Promise<unknown> | void;
  /** Row states the view starts with (a story's open state). */
  initialRows?: Rows;
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

interface Waiting {
  seq: number;
  arg: ConfigArg;
}

/**
 * The popover's Limits view (settings/anatomy.md § 5): the engine's memory as a bar, then one
 * stepper per live autolimiter knob. Each press shows at once and sends one `set_config` for that
 * knob after 400 ms without another press on it; each row keeps its own status.
 *
 * @useWhen the popover body after an open gate's Adjust limits or Limits… is pressed. It replaces the whole card, the popover's header included, and brings its own back header.
 * @states loaded, applying, applied, error, CPU warning, ceiling auto, ceiling off, unavailable, older daemon, loading
 */
export function LimitsView({ snap, invoke, onBack, onOpenSettings, onReconnect, initialRows }: LimitsViewProps) {
  const [rows, dispatch] = useReducer(
    (all: Rows, a: RowAction & { key: KnobKey }): Rows => ({ ...all, [a.key]: rowReducer(all[a.key] ?? IDLE_ROW, a) }),
    initialRows ?? {},
  );
  const [reconnecting, setReconnecting] = useState(false);
  const seqs = useRef<Partial<Record<KnobKey, number>>>({});
  const clears = useRef(new Set<ReturnType<typeof setTimeout>>());
  const wait = useRef<ReturnType<typeof debouncer<Waiting>> | null>(null);
  // One debouncer per `invoke`; closing the view (or a new invoke) sends what is waiting instead of dropping it.
  useEffect(() => {
    const timers = clears.current;
    const d = debouncer<Waiting>((key, w) => {
      const spec = KNOBS.find((k) => k.key === key) as KnobSpec;
      send(invoke, commands.setConfig({ [spec.arg]: w.arg })).then(
        () => {
          dispatch({ type: 'ok', key: key as KnobKey, seq: w.seq });
          const t = setTimeout(() => {
            timers.delete(t);
            dispatch({ type: 'clear', key: key as KnobKey, seq: w.seq });
          }, APPLIED_MS);
          timers.add(t);
        },
        (e: unknown) => dispatch({ type: 'fail', key: key as KnobKey, seq: w.seq, message: errorText(e) }),
      );
    });
    wait.current = d;
    return () => {
      d.flushAll();
      wait.current = null;
    };
  }, [invoke]);
  useEffect(() => {
    const timers = clears.current;
    return () => timers.forEach(clearTimeout);
  }, []);

  /** Shows `value` now and sends `arg` (the same number unless it is "auto"), after the wait or at once. */
  const press = (key: KnobKey, value: number | null, arg: ConfigArg, now = false) => {
    const seq = (seqs.current[key] ?? 0) + 1;
    seqs.current[key] = seq;
    dispatch({ type: 'press', key, seq, value, arg });
    const d = wait.current;
    if (d) (now ? d.flush : d.push)(key, { seq, arg });
  };
  const retry = (key: KnobKey) => {
    const r = rows[key] ?? IDLE_ROW;
    if (r.arg !== null) press(key, r.shown, r.arg, true);
  };

  const online = Boolean(snap?.online && snap.state);
  const budget = online ? snap?.state?.budget : undefined;
  const ready = limitsAvailable(budget);
  const unavailable = snap !== null && !ready;
  const segs = ready ? memorySegments(budget) : null;

  const header = (
    <ViewHeader
      title="Limits"
      onBack={onBack}
      status={
        unavailable ? undefined : (
          <>
            <span aria-hidden className="size-md rounded-full bg-green" />
            Applies now
          </>
        )
      }
    />
  );

  if (unavailable) {
    return (
      <>
        {header}
        <div className="flex flex-col items-start gap-sm px-lg py-xl">
          <p className="text-body font-semibold text-fg-strong">Limits unavailable</p>
          <p className="text-body text-muted">
            {online ? 'This faber doesn’t report its limits yet. They show once it is updated.' : 'faber is offline. Limits show once the daemon answers.'}
          </p>
          {/* Reconnecting cannot help a daemon that answers but is too old to report its limits. */}
          {!online && (
            <span className="-ml-sm">
              <Button
                variant="link"
                loading={reconnecting}
                onClick={() => {
                  setReconnecting(true);
                  Promise.resolve(onReconnect?.()).then(
                    () => setReconnecting(false),
                    () => setReconnecting(false),
                  );
                }}
              >
                Reconnect
              </Button>
            </span>
          )}
        </div>
      </>
    );
  }

  const rowsOut: ReactNode[] = KNOBS.map((spec) => {
    const row = rows[spec.key] ?? IDLE_ROW;
    const common = { step: spec.step, min: spec.min };
    if (!ready) {
      // Loading: the value is not known yet.
      return <LimitRow key={spec.key} label={spec.label} hint={spec.hint} stepper={{ ...common, value: spec.min, display: '…', busy: true, onChange: () => {} }} />;
    }
    let stepper;
    let hint = spec.hint;
    let warning: string | undefined;
    let extra: ReactNode;
    if (spec.key === 'ceiling') {
      const c = ceilingView(budget, row.shown);
      hint = c.hint;
      warning = c.warning;
      stepper = {
        ...common,
        value: c.value,
        display: c.display,
        // Off steps from nothing: + sets the resolved number, and − has nowhere to go.
        min: c.mode === 'off' ? 0 : spec.min,
        onChange: (next: number) => {
          if (c.mode !== 'off') press('ceiling', next, next);
          else if (next > 0) press('ceiling', c.fromOff, c.fromOff);
        },
      };
      if (c.canUseAuto && (row.status === 'idle' || row.status === 'applied')) {
        extra = (
          <Button size="sm" variant="link" onClick={() => press('ceiling', null, 'auto', true)}>
            Use auto
          </Button>
        );
      }
    } else {
      const value = row.shown ?? knobValue(budget, spec.key);
      stepper = {
        ...common,
        value,
        display: showKnob(spec.key, value),
        onChange: (next: number) => press(spec.key, next, next),
      };
    }
    const status = row.status !== 'idle' ? row.status : warning ? 'warning' : 'idle';
    return (
      <LimitRow
        key={spec.key}
        label={spec.label}
        hint={hint}
        stepper={stepper}
        status={status}
        message={row.status === 'error' ? row.message : warning}
        onRetry={() => retry(spec.key)}
        extra={extra}
      />
    );
  });

  return (
    <>
      {header}
      {segs && (
        <div className="border-b border-border-soft">
          <MemoryBar totalMib={segs.totalMib} reserveMib={segs.reserveMib} jobsMib={segs.jobsMib} headroomMib={segs.headroomMib} />
        </div>
      )}
      <div aria-busy={!ready || undefined} className="flex flex-col">
        {rowsOut}
      </div>
      <footer className="flex items-center justify-between rounded-b-lg bg-surface-2 px-lg py-2xs">
        {/* Pulled back by the link's own padding, so its text lines up with the rows above. */}
        <span className="-ml-sm">
          <Button size="sm" variant="link" onClick={onOpenSettings}>
            All settings…
          </Button>
        </span>
        <span aria-hidden className="text-caption text-faint">
          ⌘,
        </span>
      </footer>
    </>
  );
}
