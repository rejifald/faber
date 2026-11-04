import { Field as BaseField } from '@base-ui/react/field';
import { Input as BaseInput } from '@base-ui/react/input';
import type { LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { Icon } from './Icon';
import { cx, focusRing, ladder } from './ladder';

/** The box every text-like control shares: Input and Select's trigger, on the `md` step. */
export const controlBox = cx(
  'inline-flex items-center border border-border-control bg-surface text-fg',
  ladder.md,
  'data-disabled:opacity-50 data-invalid:border-red',
  focusRing,
);

export interface FieldProps {
  /** The field's label, always visible. */
  label: ReactNode;
  /** One line under the control saying what the value does or what form it takes. */
  description?: ReactNode;
  /** The validation message; when set, the field shows as invalid. */
  error?: string;
  /** Disables the control and dims the label, only while a sibling action in the same place runs. */
  disabled?: boolean;
  /** The control: an Input or a Select. */
  children: ReactNode;
}

/**
 * A labelled form row that ties its label, description and error to one control, wrapping
 * Base UI's Field.
 *
 * @useWhen every Input and Select in a form (the dashboard's settings). Checkbox carries its own
 * label and does not need one.
 * @states default, disabled, invalid
 */
export function Field({ label, description, error, disabled = false, children }: FieldProps) {
  return (
    <BaseField.Root disabled={disabled} invalid={Boolean(error)} className="flex flex-col gap-2xs">
      <BaseField.Label className="text-small font-semibold text-fg data-disabled:opacity-50">{label}</BaseField.Label>
      {children}
      {description && <BaseField.Description className="text-caption text-muted">{description}</BaseField.Description>}
      {error && (
        <BaseField.Error match className="text-caption text-red">
          {error}
        </BaseField.Error>
      )}
    </BaseField.Root>
  );
}

export interface InputProps {
  /** The current text, when the screen owns it (controlled). */
  value?: string;
  /** The starting text, when the input owns it (uncontrolled). */
  defaultValue?: string;
  /** Called with the new text on every edit. */
  onValueChange?: (value: string) => void;
  /** The kind of text; `number` brings the numeric keyboard and stepper. */
  type?: 'text' | 'number' | 'search' | 'url';
  /** An example of the expected form, never a substitute for the Field label. */
  placeholder?: string;
  /** Sets the text in the mono face, for paths, IDs and commands. */
  mono?: boolean;
  /** The form field name it submits under. */
  name?: string;
  /** A lucide glyph drawn inside the box at the start, such as the search field's magnifier. */
  leadingIcon?: LucideIcon;
  /** The smallest value, for `type="number"`. */
  min?: number;
  /** The step of the numeric stepper, for `type="number"`. */
  step?: number;
  /** The accessible name when no Field or visible label wraps the input. */
  'aria-label'?: string;
}

/**
 * A single-line text box on the `md` step, wrapping Base UI's Input.
 *
 * @useWhen free text or a number inside a Field. For a value from a fixed list use Select; for
 * on/off use Checkbox.
 * @states default, focus-visible, disabled, invalid
 */
export function Input({
  value,
  defaultValue,
  onValueChange,
  type = 'text',
  placeholder,
  mono = false,
  name,
  leadingIcon,
  min,
  step,
  'aria-label': ariaLabel,
}: InputProps) {
  const input = (
    <BaseInput
      type={type}
      name={name}
      value={value}
      defaultValue={defaultValue}
      placeholder={placeholder}
      min={min}
      step={step}
      aria-label={ariaLabel}
      onValueChange={(next) => onValueChange?.(String(next))}
      className={cx(controlBox, 'w-full placeholder:text-faint', mono ? 'font-mono' : 'font-sans', leadingIcon && 'pl-4xl')}
    />
  );
  if (!leadingIcon) return input;
  return (
    <span className="relative block w-full">
      <span aria-hidden className="pointer-events-none absolute inset-y-0 left-md flex items-center text-muted">
        <Icon icon={leadingIcon} />
      </span>
      {input}
    </span>
  );
}
