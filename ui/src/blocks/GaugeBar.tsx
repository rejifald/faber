import type { GaugeModel } from '../popover/gauges';

// The bar's corner radius and the fit mark's stroke, in px: geometry inside the drawn mark
// (guidelines § Scales). The bar's height is the theme's `h-gauge`.
const RADIUS = 1.5;
const MARK = 2;

const fillTone = (g: GaugeModel) =>
  g.bind === 'ok' ? 'fill-accent' : g.bind === 'warn' ? 'fill-amber' : g.off ? 'fill-faint' : 'fill-muted';
const captionTone = (g: GaugeModel) =>
  g.bind === 'ok' ? 'font-semibold text-accent-text' : g.bind === 'warn' ? 'font-semibold text-amber' : 'text-muted';

/** A fraction as an SVG length: a share of the bar's width, whatever the column is. */
const pct = (f: number) => `${f * 100}%`;

/** A full-width rounded track, or one slot's segment of it. */
function Track({ className }: { className: string }) {
  return (
    <svg width="100%" aria-hidden className="block h-gauge min-w-0 flex-1">
      <rect width="100%" height="100%" rx={RADIUS} className={className} />
    </svg>
  );
}

/** The bar: one segment per slot, or a track with its fill and the fit mark. */
function Bar({ gauge }: { gauge: GaugeModel }) {
  if (gauge.segs) {
    return (
      <span aria-hidden className="flex gap-3xs">
        {Array.from({ length: gauge.segs }, (_, i) => (
          <Track key={i} className={i < (gauge.filled ?? 0) ? fillTone(gauge) : 'fill-border'} />
        ))}
      </span>
    );
  }
  return (
    <svg width="100%" aria-hidden className="block h-gauge overflow-visible">
      <rect width="100%" height="100%" rx={RADIUS} className="fill-border" />
      <rect width={pct(gauge.fill ?? 0)} height="100%" rx={RADIUS} className={fillTone(gauge)} />
      {gauge.mark != null && (
        <line x1={pct(gauge.mark)} x2={pct(gauge.mark)} y1={0} y2="100%" strokeWidth={MARK} className="stroke-fg-strong" />
      )}
    </svg>
  );
}

export interface GaugeBarProps {
  /** The gate: its caption, figures, fill, fit mark and whether it binds. Omit for the loading frame's empty track. */
  gauge?: GaugeModel;
  /** The loading frame's caption, shown over an empty track before the first snapshot. */
  placeholder?: string;
}

/**
 * One gate in force as a row: its name, marked "binds" on the gate that limits the next job, its
 * real figures ("5.5 / 8.0 GiB"), and a 6 px bar under them. The binding gate's bar takes the
 * accent, or amber while something waits; a CPU gate not in `auto` is faint. Slots draw one
 * segment per slot. A tick marks where the next typical job would no longer fit. It has no
 * padding of its own: the gate row that holds it sets that.
 *
 * @useWhen the summary of a GateRow, one per gate in force (popover.md § Hero); alone, the loading frame.
 * @states default, binding, binding while something waits, not gated, slots, slots binding, no fit mark, loading
 */
export function GaugeBar({ gauge, placeholder }: GaugeBarProps) {
  if (!gauge) {
    return (
      <div className="flex flex-col gap-2xs">
        <span className="flex items-center gap-sm">
          <span className="min-w-0 flex-1 truncate text-small text-muted">{placeholder}</span>
          <span className="h-lg w-1/4 rounded-full bg-surface-2" />
        </span>
        <span aria-hidden className="flex">
          <Track className="fill-border" />
        </span>
      </div>
    );
  }
  return (
    <div role="img" aria-label={gauge.aria} title={gauge.aria} className="flex flex-col gap-2xs">
      <span className="flex items-baseline gap-sm">
        {/* The name gives way before "binds": the marker is what says this gate limits the next job. */}
        <span className={`flex min-w-0 flex-1 text-left text-small ${captionTone(gauge)}`}>
          <span className="min-w-0 truncate">{gauge.caption}</span>
          {gauge.bind && <span className="shrink-0 whitespace-pre"> binds</span>}
        </span>
        <span className={`shrink-0 font-mono text-caption whitespace-nowrap ${gauge.off ? 'text-faint' : 'text-fg'}`}>{gauge.value}</span>
      </span>
      <Bar gauge={gauge} />
    </div>
  );
}
