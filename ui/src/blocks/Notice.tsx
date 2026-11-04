import { CircleAlert } from 'lucide-react';
import type { ReactNode } from 'react';
import { Icon } from '../kit';

export interface NoticeProps {
  /** `amber` for a state to act on (restart pending, form changes unsaved, stale); `red` for a refusal. */
  tone: 'amber' | 'red';
  /** One line naming what happened. */
  title: string;
  /** What it means and what to do. */
  body?: ReactNode;
  /** A command the operator runs, in mono (`sudo faber config apply config.toml`). */
  command?: string;
  /** Kit Buttons, right-aligned under the text. */
  actions?: ReactNode;
}

const ink = { amber: 'text-amber', red: 'text-red' } as const;
const tones = { amber: 'bg-amber-soft', red: 'bg-red-soft' } as const;

/**
 * An inline notice in the Settings view: a tinted block with a title, a sentence, an optional
 * command and its actions.
 *
 * @useWhen the view must tell the operator a save was refused, form changes block the text editor,
 * or a restart is pending. Not for a field's own error (Field shows that).
 * @states amber, red, with command, with actions
 */
export function Notice({ tone, title, body, command, actions }: NoticeProps) {
  return (
    <section role={tone === 'red' ? 'alert' : 'status'} className={`flex flex-col gap-xs rounded-md px-lg py-md ${tones[tone]}`}>
      <h3 className="flex items-center gap-xs text-body font-semibold text-fg-strong">
        <span className={ink[tone]}>
          <Icon icon={CircleAlert} />
        </span>
        {title}
      </h3>
      {body && <p className="text-small text-muted">{body}</p>}
      {command && <code className="rounded-sm bg-surface-2 px-sm py-2xs font-mono text-small text-fg">{command}</code>}
      {actions && <div className="flex justify-end gap-sm">{actions}</div>}
    </section>
  );
}
