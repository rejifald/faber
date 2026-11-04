import { Meter as BaseMeter } from "@base-ui/react/meter";
import type { ReactNode } from "react";
import type { PassThrough } from "./ladder";

export interface MeterProps extends PassThrough<HTMLDivElement> {
  /** The measure. Above `max` it is clamped for assistive technology; `valueText` still says the truth. */
  value: number;
  /** The top of the scale. */
  max: number;
  /** The bottom of the scale. */
  min?: number;
  /** The accessible name: what is measured ("Memory budget"). */
  label: string;
  /** What a screen reader says for the value ("12.0 of 18.0 GiB reserved"), written by the caller. */
  valueText: string;
  /** The drawn mark. Hidden from assistive technology: the meter carries the value. */
  children: ReactNode;
}

/**
 * An accessible value for a measure the caller draws, wrapping Base UI's Meter. It draws nothing of
 * its own: the children are the picture, hidden from assistive technology, and the meter role
 * carries the name and the value text.
 *
 * @useWhen a drawn bar or rail shows a measure within a known range and needs a spoken value. For a
 * task with progress toward done use a progress bar; for a number alone no meter is needed.
 * @states default, full (value at max), over max (ARIA value clamped to max, valueText says the truth)
 */
export function Meter({
  value,
  max,
  min = 0,
  label,
  valueText,
  children,
  ...rest
}: MeterProps) {
  const clamped = Math.min(Math.max(value, min), max);
  return (
    <BaseMeter.Root
      {...rest}
      value={clamped}
      min={min}
      max={max}
      aria-label={label}
      aria-valuetext={valueText}
    >
      <div aria-hidden>{children}</div>
    </BaseMeter.Root>
  );
}
