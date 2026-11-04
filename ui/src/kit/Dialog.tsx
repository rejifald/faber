import { Dialog as BaseDialog } from '@base-ui/react/dialog';
import type { ReactNode } from 'react';

export interface DialogProps {
  /** Whether the dialog is showing; the screen owns it. */
  open: boolean;
  /** Called with the next value when Escape, a press on the backdrop or a close control asks for it. */
  onOpenChange: (open: boolean) => void;
  /** The heading; it also names the dialog for a screen reader. */
  title: string;
  /** One line under the title saying what the answer is for. */
  description?: string;
  /** The form: Fields, the first of which takes focus when the dialog opens. */
  children: ReactNode;
  /** The Buttons of the footer row, right-aligned, one `primary`. While the answer is sent the primary is `loading`. */
  actions: ReactNode;
  /** A failure from the last attempt, read out as a status; the dialog stays open so the answer can be corrected. */
  error?: string;
}

/**
 * A short form that must be answered before an action runs, wrapping Base UI's Dialog. It is modal:
 * focus moves to the first field, Escape or a press on the backdrop closes it and focus returns to
 * where it was.
 *
 * @useWhen an action needs one answer first (the reason for holding Docker). Not for read-only
 * content (use Popover) and not for a confirmation that fits a row.
 * @states closed, open, submitting, error
 */
export function Dialog({ open, onOpenChange, title, description, children, actions, error }: DialogProps) {
  return (
    <BaseDialog.Root open={open} onOpenChange={onOpenChange}>
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className="fixed inset-0 bg-bg/70" />
        <BaseDialog.Viewport className="fixed inset-0 flex items-center justify-center p-xl">
          <BaseDialog.Popup className="flex w-popover flex-col gap-lg rounded-lg border border-border bg-elevate p-xl shadow-floating outline-hidden">
            <div className="flex flex-col gap-2xs">
              <BaseDialog.Title className="text-title font-semibold text-fg-strong">{title}</BaseDialog.Title>
              {description && (
                <BaseDialog.Description className="text-small text-muted">{description}</BaseDialog.Description>
              )}
            </div>
            {children}
            {error && (
              <p role="status" className="text-small text-red">
                {error}
              </p>
            )}
            <div className="flex justify-end gap-sm">{actions}</div>
          </BaseDialog.Popup>
        </BaseDialog.Viewport>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}
