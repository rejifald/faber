import type { ReactNode } from 'react';
import { Button, Stepper, type StepperProps } from '../kit';
import { cut } from '../popover/limits';

export interface LimitRowProps {
  /** The knob's name. */
  label: string;
  /** What it limits, shown under the name while nothing else needs saying. */
  hint: string;
  /** The stepper's props; the row sets its `tone` from `status`, and `busy` while applying (or when the caller sets it, as while loading). */
  stepper: Omit<StepperProps, 'tone' | 'label'>;
  /** The last change: `warning` is a standing note on the value, the rest follow one press. */
  status?: 'idle' | 'applying' | 'applied' | 'error' | 'warning';
  /** The status text for `warning`, or the daemon's reason for `error`; the daemon's text is set as text. */
  message?: string;
  /** Sends the failed change again; shown as Retry in place of the stepper. */
  onRetry?: () => void;
  /** Sits after the hint on its line: the *Use auto* link button. */
  extra?: ReactNode;
}

const LINE = {
  idle: 'text-muted',
  applying: 'text-muted',
  applied: 'text-green',
  error: 'text-red',
  warning: 'text-amber',
} as const;

/**
 * One live knob of the Limits view: its name and hint on the left, a Stepper on the right, and the
 * status of the last change in the hint line. An error tints the row and swaps the Stepper for Retry.
 *
 * @useWhen one autolimiter knob that applies as it is pressed. For a setting saved with a button, use a form field.
 * @states idle, applying, applied, error, warning
 */
export function LimitRow({ label, hint, stepper, status = 'idle', message, onRetry, extra }: LimitRowProps) {
  let line = hint;
  if (status === 'applying') line = 'Applying…';
  else if (status === 'applied') line = 'Applied';
  else if (status === 'warning' && message) line = message;
  else if (status === 'error') line = `Not applied: ${cut(message ?? 'no reason given')}`;
  return (
    <div className={`flex items-center gap-sm border-b border-border-soft px-lg py-sm ${status === 'error' ? 'bg-red-soft' : ''}`}>
      <div className="flex min-w-0 flex-1 flex-col">
        <span className="text-small font-medium text-fg">{label}</span>
        <span className="flex flex-wrap items-center gap-x-sm text-caption">
          <span role="status" title={status === 'error' ? message : undefined} className={`wrap-anywhere ${LINE[status]}`}>
            {line}
          </span>
          {/* A link button's own padding would indent it past the hint; pull it back by that step. */}
          {extra && <span className="-ml-sm">{extra}</span>}
        </span>
      </div>
      {status === 'error' ? (
        <Button variant="link" aria-label={`Retry ${label}`} onClick={() => onRetry?.()}>
          Retry
        </Button>
      ) : (
        <Stepper
          {...stepper}
          label={label}
          busy={status === 'applying' || stepper.busy}
          tone={status === 'applied' ? 'success' : status === 'warning' ? 'warning' : 'default'}
        />
      )}
    </div>
  );
}
