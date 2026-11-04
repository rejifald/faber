import { Button } from '../kit';

export interface ConfirmRowProps {
  /** The question, "Remove linux?". */
  question: string;
  /** What follows from saying yes. */
  consequence: string;
  /** The destructive action's label. */
  confirmLabel?: string;
  /** Keeps the item; the row closes. */
  onCancel: () => void;
  /** Removes the item. */
  onConfirm: () => void;
}

/**
 * An in-place confirmation of a destructive action, drawn where the row it concerns sits.
 *
 * @useWhen Remove on a profile, before anything is saved. The danger Button is the confirmed action.
 * @states default
 */
export function ConfirmRow({ question, consequence, confirmLabel = 'Remove', onCancel, onConfirm }: ConfirmRowProps) {
  return (
    <section role="alertdialog" aria-label={question} className="flex flex-col gap-xs rounded-md bg-red-soft px-lg py-md">
      <h3 className="text-body font-semibold text-fg-strong">{question}</h3>
      <p className="text-small text-muted">{consequence}</p>
      <div className="flex justify-end gap-sm">
        <Button variant="link" onClick={onCancel}>Cancel</Button>
        <Button variant="danger" onClick={onConfirm}>
          {confirmLabel}
        </Button>
      </div>
    </section>
  );
}
