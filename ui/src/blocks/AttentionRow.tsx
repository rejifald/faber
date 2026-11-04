import { useEffect, useId, useRef, useState, type KeyboardEvent } from 'react';
import { Download, Info, Pause, Snowflake, TriangleAlert, X, type LucideIcon } from 'lucide-react';
import { Button, Icon, IconButton } from '../kit';
import { DISMISS, MARK, plainText, type AttentionIcon, type AttentionRow as Row, type Part, type RowAction, type Severity } from '../popover/attention';
import { Pipe } from './Pipe';

/** Each row glyph's lucide icon; the dismissed list draws the same marks. */
export const rowIcons: Record<AttentionIcon, LucideIcon> = {
  alert: TriangleAlert,
  pause: Pause,
  frozen: Snowflake,
  info: Info,
  update: Download,
};

/** The band's fill and its icon's colour, by severity (guidelines.md, Colour). */
const BAND: Record<Severity, string> = { bad: 'bg-red-soft', warn: 'bg-amber-soft', info: 'bg-surface-2' };

/** One run of a row's words, set as text: the head bold, a value mono, a pipe between independent values. */
export function text(p: Part, i: number) {
  if (typeof p === 'string') return p;
  if ('strong' in p) return <strong key={i} className="font-bold text-fg-strong">{p.strong}</strong>;
  if ('mono' in p) return <span key={i} className="font-mono text-caption">{p.mono}</span>;
  return <Pipe key={i} />;
}

export interface AttentionRowProps {
  /** The issue: severity, glyph, words and its actions. Daemon text arrives as string parts and is set as text. */
  row: Row;
  /** The label of this row's action that is in flight: it shows loading and the row's other actions wait. */
  pending?: string;
  /** What the last press of an action reported, until the row goes away or the action is pressed again. */
  error?: string;
  /** Runs an action; the screen tracks it as pending until it settles. */
  onAction?: (action: RowAction) => void;
  /** Hides the row (ADR-0064); only a row that does not stop jobs gets it. Pending as `Dismiss`. */
  onDismiss?: () => void;
  /** Whether a row with `details` starts open (a story's open state). */
  defaultDetailsOpen?: boolean;
}

/**
 * One active issue in the popover's attention list: a full-bleed band tinted by severity (red,
 * amber, neutral), its glyph, the issue in one or two sentences, clamped at three lines with the
 * whole text on hover, and its inline actions on the `sm` step.
 *
 * @useWhen the popover's attention list, one per active issue, worst first (popover.md). Not for
 * the healthy state: a healthy popover shows no row.
 * An action with `confirm` asks in place first: its question and consequence, Cancel and the
 * confirmed action, where the actions were. Focus moves to Cancel, Escape cancels, and either
 * answer hands focus back to the action that asked.
 *
 * A row that does not stop jobs carries a quiet Dismiss (×) at its end (ADR-0064). A row with
 * `details` (acknowledged trust changes) opens in place to list them.
 *
 * @states bad, warn, info, with actions, action pending, action failed, confirming, dismissable, dismiss pending, dismiss failed, with details, details open
 */
export function AttentionRow({ row, pending, error, onAction, onDismiss, defaultDetailsOpen = false }: AttentionRowProps) {
  const [asking, setAsking] = useState<RowAction | null>(null);
  const [detailsOpen, setDetailsOpen] = useState(defaultDetailsOpen);
  const detailsId = useId();
  const consequenceId = useId();
  const cancel = useRef<HTMLButtonElement>(null);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  // The action whose confirmation just closed: it takes focus back once it is drawn again.
  const returnTo = useRef<string | null>(null);
  useEffect(() => {
    if (asking) cancel.current?.focus();
    else if (returnTo.current) {
      buttons.current.get(returnTo.current)?.focus();
      returnTo.current = null;
    }
  }, [asking]);
  const press = (a: RowAction) => (a.confirm ? setAsking(a) : onAction?.(a));
  const close = (run: boolean) => {
    if (!asking) return;
    returnTo.current = asking.label;
    setAsking(null);
    if (run) onAction?.(asking);
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      close(false);
    }
  };
  return (
    <div className={`flex gap-sm px-xl py-sm text-small text-fg ${BAND[row.sev]}`} title={plainText(row.parts)}>
      <span className={MARK[row.sev]}>
        <Icon icon={rowIcons[row.icon]} size="md" leading />
      </span>
      <div className="flex min-w-0 flex-1 flex-col gap-xs">
        <p className="line-clamp-3 wrap-anywhere">{row.parts.map(text)}</p>
        {asking?.confirm && (
          <section
            role="alertdialog"
            aria-label={asking.confirm.question}
            aria-describedby={consequenceId}
            onKeyDown={onKeyDown}
            className="flex flex-col gap-xs"
          >
            <p className="font-semibold text-fg-strong">{asking.confirm.question}</p>
            <p id={consequenceId} className="text-muted">
              {asking.confirm.consequence}
            </p>
            <div className="flex items-center gap-sm">
              <Button ref={cancel} size="sm" variant="link" onClick={() => close(false)}>
                Cancel
              </Button>
              <Button size="sm" variant="primary" onClick={() => close(true)}>
                {asking.confirm.label}
              </Button>
            </div>
          </section>
        )}
        {row.details && detailsOpen && (
          <ul id={detailsId} className="flex flex-col gap-2xs text-muted">
            {row.details.lines.map((line, i) => (
              <li key={i} className="wrap-anywhere">
                {line.map(text)}
              </li>
            ))}
          </ul>
        )}
        {row.details && (
          <div className="flex items-center">
            <Button size="sm" variant="link" aria-expanded={detailsOpen} aria-controls={detailsId} onClick={() => setDetailsOpen(!detailsOpen)}>
              {detailsOpen ? `Hide ${row.details.label}` : `Show ${row.details.label}`}
            </Button>
          </div>
        )}
        {!asking && ((row.actions && row.actions.length > 0) || (onDismiss && error)) && (
          <div className="flex flex-wrap items-center gap-sm">
            {(row.actions || []).map((a) => (
              <Button
                key={a.label}
                ref={(el: HTMLButtonElement | null) => {
                  if (el) buttons.current.set(a.label, el);
                  else buttons.current.delete(a.label);
                }}
                size="sm"
                variant={a.primary ? 'primary' : 'secondary'}
                loading={pending === a.label}
                disabled={pending !== undefined && pending !== a.label}
                onClick={() => press(a)}
              >
                {a.label}
              </Button>
            ))}
            <span role="status" className="text-caption text-red">{error}</span>
          </div>
        )}
      </div>
      {onDismiss && !asking && (
        <span className="flex shrink-0 items-start">
          <IconButton
            icon={X}
            size="sm"
            label={DISMISS}
            loading={pending === DISMISS}
            disabled={pending !== undefined && pending !== DISMISS}
            onClick={onDismiss}
          />
        </span>
      )}
    </div>
  );
}
