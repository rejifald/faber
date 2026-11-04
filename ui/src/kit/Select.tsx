import { Select as BaseSelect } from '@base-ui/react/select';
import { Check, ChevronsUpDown } from 'lucide-react';
import { controlBox } from './Field';
import { cx, focusRing, iconSize, ladder } from './ladder';

/** One option: the value it stands for and what the list shows. */
export interface SelectOption {
  value: string;
  label: string;
}

export interface SelectProps {
  /** The options, in the order shown. */
  options: SelectOption[];
  /** The chosen value, when the screen owns it (controlled). */
  value?: string;
  /** The value chosen first, when Select owns it (uncontrolled). */
  defaultValue?: string;
  /** Called with the newly chosen value. */
  onValueChange?: (value: string) => void;
  /** Shown while nothing is chosen. */
  placeholder?: string;
  /** The form field name it submits under. */
  name?: string;
}

/**
 * A button that opens a list to pick one value from, on the `md` step, wrapping Base UI's Select.
 *
 * @useWhen one value from a short fixed list, inside a Field. For actions use Menu; for two
 * states use Checkbox; for peer views use Tabs.
 * @states closed, open, item highlighted, item selected, disabled, invalid
 */
export function Select({ options, value, defaultValue, onValueChange, placeholder, name }: SelectProps) {
  return (
    <BaseSelect.Root
      items={options}
      name={name}
      value={value}
      defaultValue={defaultValue}
      onValueChange={(next) => next !== null && onValueChange?.(String(next))}
    >
      <BaseSelect.Trigger className={cx(controlBox, 'justify-between font-sans cursor-default select-none')}>
        <BaseSelect.Value placeholder={placeholder} className="data-placeholder:text-faint" />
        <BaseSelect.Icon className="flex text-muted">
          <ChevronsUpDown aria-hidden className={iconSize.md} />
        </BaseSelect.Icon>
      </BaseSelect.Trigger>
      <BaseSelect.Portal>
        <BaseSelect.Positioner sideOffset={4} alignItemWithTrigger={false}>
          <BaseSelect.Popup className="rounded-md border border-border bg-elevate p-2xs shadow-floating outline-hidden">
            <BaseSelect.List>
              {options.map((option) => (
                <BaseSelect.Item
                  key={option.value}
                  value={option.value}
                  className={cx(
                    'flex items-center justify-between select-none cursor-default text-fg',
                    ladder.md,
                    'data-highlighted:bg-surface-2 data-disabled:opacity-50',
                    focusRing,
                  )}
                >
                  <BaseSelect.ItemText>{option.label}</BaseSelect.ItemText>
                  <BaseSelect.ItemIndicator className="flex text-accent-text">
                    <Check aria-hidden className={iconSize.md} />
                  </BaseSelect.ItemIndicator>
                </BaseSelect.Item>
              ))}
            </BaseSelect.List>
          </BaseSelect.Popup>
        </BaseSelect.Positioner>
      </BaseSelect.Portal>
    </BaseSelect.Root>
  );
}
