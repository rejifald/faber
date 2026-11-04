import { CircleAlert, TriangleAlert, X } from 'lucide-react';
import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { Button, Icon, IconButton } from '../kit';
import { DISMISS } from '../popover/attention';

/** One action on the card. */
export interface AttentionCardAction {
  /** The verb on the button. */
  label: string;
  /** Runs the action; the screen tracks it as pending until it settles. */
  onClick: () => void;
  /** The main action, the card's one filled button. With none marked, the first is; the rest are links. */
  primary?: boolean;
}

/** A question asked in the card before an action that accepts a risk runs (#385). */
export interface AttentionCardConfirm {
  question: string;
  consequence: string;
  /** The confirmed action's verb. */
  label: string;
  onCancel: () => void;
  onConfirm: () => void;
}

export interface AttentionCardProps {
  /** The issue's severity: red a fault, amber a caution, neutral for information. */
  tone: 'red' | 'amber' | 'neutral';
  /** The issue in a few words. */
  title: string;
  /** What happened and what it needs. Daemon text is passed as strings and set as text. */
  body: ReactNode;
  /** What the operator can do about it. */
  actions?: AttentionCardAction[];
  /** The label of the action that is in flight: it shows loading and the others wait. */
  pending?: string;
  /** What the last press of an action reported, until the card goes away or the action is pressed again. */
  error?: string;
  /** Asks in place, where the actions were. Focus lands on Cancel; Escape cancels. */
  confirm?: AttentionCardConfirm;
  /** Hides the card (ADR-0064): only for an issue that does not stop jobs. In flight while `pending` is `Dismiss`. */
  onDismiss?: () => void;
  /** Lines the card opens to list under "Show <label>": the trust changes an acknowledgement folded. */
  details?: { label: string; lines: ReactNode[] };
  /** Whether `details` start open (a story's open state). */
  defaultDetailsOpen?: boolean;
}

const BORDER = { red: 'border-red-soft', amber: 'border-amber-soft', neutral: 'border-border' };
const INK = { red: 'text-red', amber: 'text-amber', neutral: 'text-muted' };

/**
 * One active issue on the dashboard that needs the operator, as a card: a tinted border, its
 * glyph and title, what happened, and the actions to take. The popover's attention row, with
 * the same severities, laid out for the page.
 *
 * @useWhen the dashboard has an issue that needs a decision or a fix, one card per issue, worst
 * first. Not for the popover (AttentionRow) and not for a healthy host: show no card.
 * An issue that does not stop jobs carries a quiet Dismiss (×) beside its title (ADR-0064).
 * @states amber, red, neutral, with actions, action pending, action failed, dismissable, dismiss pending, dismiss failed, with details, details open
 */
export function AttentionCard({ tone, title, body, actions, pending, error, confirm, onDismiss, details, defaultDetailsOpen = false }: AttentionCardProps) {
  const titleId = useId();
  const detailsId = useId();
  const [detailsOpen, setDetailsOpen] = useState(defaultDetailsOpen);
  const consequenceId = useId();
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (confirm) cancel.current?.focus();
  }, [confirm]);
  // One filled primary per place: the first marked action, else the first.
  const marked = actions?.findIndex((a) => a.primary) ?? -1;
  const main = marked < 0 ? 0 : marked;
  return (
    <div role="group" aria-labelledby={titleId} className={`flex flex-col gap-sm rounded-lg border bg-surface px-xl py-lg ${BORDER[tone]}`}>
      <div className="flex items-start gap-sm">
        <p id={titleId} className={`flex min-w-0 flex-1 items-center gap-sm text-small font-semibold ${INK[tone]}`}>
          <Icon icon={tone === 'neutral' ? CircleAlert : TriangleAlert} size="sm" />
          {title}
        </p>
        {onDismiss && !confirm && (
          <IconButton
            icon={X}
            size="sm"
            label={DISMISS}
            loading={pending === DISMISS}
            disabled={pending !== undefined && pending !== DISMISS}
            onClick={onDismiss}
          />
        )}
      </div>
      <div className="text-small text-fg wrap-anywhere">{body}</div>
      {details && detailsOpen && (
        <ul id={detailsId} className="flex flex-col gap-2xs text-small text-muted">
          {details.lines.map((line, i) => (
            <li key={i} className="wrap-anywhere">
              {line}
            </li>
          ))}
        </ul>
      )}
      {details && (
        <div className="flex items-center">
          <Button size="sm" variant="link" aria-expanded={detailsOpen} aria-controls={detailsId} onClick={() => setDetailsOpen(!detailsOpen)}>
            {detailsOpen ? `Hide ${details.label}` : `Show ${details.label}`}
          </Button>
        </div>
      )}
      {confirm && (
        <section
          role="alertdialog"
          aria-label={confirm.question}
          aria-describedby={consequenceId}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.stopPropagation();
              confirm.onCancel();
            }
          }}
          className="flex flex-col gap-xs"
        >
          <p className="font-semibold text-fg-strong">{confirm.question}</p>
          <p id={consequenceId} className="text-small text-muted">
            {confirm.consequence}
          </p>
          <div className="flex items-center gap-sm">
            <Button ref={cancel} size="sm" variant="link" onClick={confirm.onCancel}>
              Cancel
            </Button>
            <Button size="sm" variant="primary" onClick={confirm.onConfirm}>
              {confirm.label}
            </Button>
          </div>
        </section>
      )}
      {!confirm && ((actions && actions.length > 0) || (onDismiss && error)) && (
        <div className="flex flex-wrap items-center gap-sm">
          {(actions || []).map((a, i) => (
            <Button
              key={a.label}
              size="sm"
              variant={i === main ? 'primary' : 'link'}
              loading={pending === a.label}
              disabled={pending !== undefined && pending !== a.label}
              onClick={a.onClick}
            >
              {a.label}
            </Button>
          ))}
          <span role="status" className="text-caption text-red">{error}</span>
        </div>
      )}
    </div>
  );
}
