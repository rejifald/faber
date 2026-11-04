import { Button, Disclosure } from '../kit';
import type { GateFact, GaugeModel } from '../popover/gauges';
import { FactList } from './FactList';
import { GaugeBar } from './GaugeBar';

export interface GateRowProps {
  /** The gate: its bar, caption and figures, and its facts in words for the row's name. */
  gauge: GaugeModel;
  /** What the open row lists under the bar (`gateFacts`), only the lines whose fields the daemon sent. */
  facts: GateFact[];
  /** Whether its detail shows. One gate is open at a time; the screen keeps which. */
  open: boolean;
  /** Called when the row is pressed. */
  onOpenChange: (open: boolean) => void;
  /** Opens the Limits view. */
  onAdjust: () => void;
}

/**
 * One gate as a row that opens in place, like a job group: its bar is the row, and pressing it
 * lists what the bar is made of (reserved, free, what the next job takes, the floor …), an amber
 * line when claims wait at it, and Adjust limits into the Limits view.
 *
 * @useWhen the popover's gauges, one per gate in force (popover.md § Hero). One open at a time.
 * @states closed, hover, open, open while jobs wait, focus-visible
 */
export function GateRow({ gauge, facts, open, onOpenChange, onAdjust }: GateRowProps) {
  const figures = facts.filter((f) => !f.waits && f.value);
  const waits = facts.filter((f) => f.waits);
  return (
    <Disclosure summary={<GaugeBar gauge={gauge} />} label={gauge.aria} open={open} onOpenChange={onOpenChange}>
      <FactList facts={figures.map((f) => ({ label: f.label, value: f.value ?? '' }))} />
      {/* A wait line is a sentence, not a term with a value: it sits after the list. */}
      {waits.map((f) => (
        <p key={f.label} className="truncate px-xl pt-3xs text-small text-amber">
          {f.label}
        </p>
      ))}
      <span className="flex px-sm pt-2xs">
        <Button size="sm" variant="link" onClick={onAdjust}>
          Adjust limits
        </Button>
      </span>
    </Disclosure>
  );
}
