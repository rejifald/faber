import { Switch as BaseSwitch } from '@base-ui/react/switch';
import { cx, focusRing, iconSize, type PassThrough, type Size } from './ladder';

export interface SwitchProps extends PassThrough<HTMLSpanElement> {
  /** The accessible name: what turning it on means ("Git mirror"). The row beside it shows the visible label. */
  label: string;
  /** The ladder step: the track is as tall as the step's icon and 1.75 times as wide. */
  size?: Size;
  /** Whether it is on. */
  checked: boolean;
  /** Called with the new state. */
  onCheckedChange: (checked: boolean) => void;
  /** Shows it but refuses changes, only while a sibling action in the same place runs. */
  disabled?: boolean;
}

const trackWidth: Record<Size, string> = {
  sm: 'w-switch-sm',
  md: 'w-switch-md',
  lg: 'w-switch-lg',
};

/**
 * An on/off setting drawn as a track and thumb, wrapping Base UI's Switch.
 *
 * @useWhen a setting that is neither optional nor `sudo`, in a row that carries its own label
 * and shows an unsaved change itself. For a box beside its label use Checkbox; for a choice of
 * more than two, or a setting that can be unstated, use Select.
 * @states off, on, hover, focus-visible, pressed, disabled
 */
export function Switch({ label, size = 'md', checked, onCheckedChange, disabled = false, ...rest }: SwitchProps) {
  return (
    <BaseSwitch.Root
      {...rest}
      aria-label={label}
      checked={checked}
      onCheckedChange={(next) => onCheckedChange(next)}
      disabled={disabled}
      className={cx(
        'inline-flex shrink-0 items-center justify-start rounded-full p-3xs transition-colors',
        iconSize[size],
        trackWidth[size],
        'bg-border-control hover:brightness-95 active:brightness-90',
        'data-checked:justify-end data-checked:bg-green',
        'data-disabled:opacity-50',
        focusRing,
      )}
    >
      <BaseSwitch.Thumb className="aspect-square h-full rounded-full bg-surface" />
    </BaseSwitch.Root>
  );
}
