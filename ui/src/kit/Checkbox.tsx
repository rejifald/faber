import { Checkbox as BaseCheckbox } from '@base-ui/react/checkbox';
import { Check } from 'lucide-react';
import type { ReactNode } from 'react';
import { boxed, cx, focusRing, iconSize, type PassThrough, type Size } from './ladder';

export interface CheckboxProps extends PassThrough<HTMLSpanElement> {
  /** What turning it on means, as a statement ("Keep the Mac awake"). */
  label: ReactNode;
  /** The ladder step: label size, gap and a box the size of the step's icon (`md` is 12 px, 6 px, 14 px). */
  size?: Size;
  /** Whether it is on, when the screen owns it (controlled). */
  checked?: boolean;
  /** Whether it starts on, when Checkbox owns it (uncontrolled). */
  defaultChecked?: boolean;
  /** Called with the new state. */
  onCheckedChange?: (checked: boolean) => void;
  /** Shows it but refuses changes, only while a sibling action in the same place runs. */
  disabled?: boolean;
  /** The form field name it submits under. */
  name?: string;
}

/**
 * An on/off box with its label, wrapping Base UI's Checkbox. The label is part of the hit area.
 *
 * @useWhen a setting with two states that applies on save. For a choice of more than two use
 * Select; for an action that happens at once use Button.
 * @states unchecked, checked, focus-visible, disabled
 */
export function Checkbox({
  label,
  size = 'md',
  checked,
  defaultChecked,
  onCheckedChange,
  disabled = false,
  name,
  ...rest
}: CheckboxProps) {
  return (
    <label className={cx('inline-flex items-center text-fg select-none has-data-disabled:opacity-50', boxed[size])}>
      <BaseCheckbox.Root
        {...rest}
        name={name}
        checked={checked}
        defaultChecked={defaultChecked}
        onCheckedChange={(next) => onCheckedChange?.(next)}
        disabled={disabled}
        className={cx(
          'inline-flex shrink-0 items-center justify-center rounded-xs border border-border-control bg-surface',
          iconSize[size],
          'data-checked:border-accent-text data-checked:bg-accent-text data-checked:text-surface',
          focusRing,
        )}
      >
        <BaseCheckbox.Indicator className="flex">
          <Check aria-hidden className="size-full" />
        </BaseCheckbox.Indicator>
      </BaseCheckbox.Root>
      {label}
    </label>
  );
}
