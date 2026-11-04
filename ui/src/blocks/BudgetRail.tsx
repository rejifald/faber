import { useId } from 'react';
import { gib } from '../popover/limits';
import { Meter } from '../kit';
import { Pipe } from './Pipe';

export interface BudgetRailJob {
  /** Stable key for the list. */
  key: string;
  /** What holds the slice, as text: "acme/web e2e". */
  label: string;
  /** Memory this job has reserved, MiB. */
  mib: number;
}

export interface BudgetRailProps {
  /** The admission budget in MiB, reserve included. */
  budgetMib: number;
  /** Kept for the VM itself, MiB. Part of the budget. */
  reserveMib: number;
  /** Jobs holding memory, largest first. */
  jobs: BudgetRailJob[];
  /** Jobs admitted and running. */
  admitted: number;
  /** Jobs waiting for memory. Amber when above zero. */
  parked: number;
  /** The autolimiter's safety margin, the factor learned caps are multiplied by; shown as `margin 1.15`. */
  safetyMargin?: number;
}

/** Width of the gap between segments, in px (SVG user units). */
const GAP = 2;
/** A job under this share of the rail merges into "N more". */
const MERGE_BELOW = 0.01;

/**
 * The memory budget as one segmented rail: a segment per job holding memory, then what is free to
 * admit, then the reserve, with the reserved figure and a legend of the numbers in GiB. Jobs share
 * one colour and are told apart by the legend and each segment's title. Jobs under 1 % merge into
 * one "N more" segment.
 *
 * @useWhen the dashboard Overview shows who is using the memory budget. For the Limits strip's four-part bar use MemoryBar.
 * @states default, no jobs (rail is free and reserve, legend omits jobs), over budget (free absent, reserved figure amber), many jobs (legend wraps, tiny jobs merge into "N more")
 */
export function BudgetRail({
  budgetMib,
  reserveMib,
  jobs,
  admitted,
  parked,
  safetyMargin,
}: BudgetRailProps) {
  const mask = useId();
  const reservedMib = jobs.reduce((s, j) => s + j.mib, 0);
  const freeMib = Math.max(0, budgetMib - reservedMib - reserveMib);
  const over = reservedMib + reserveMib > budgetMib;
  const total = Math.max(budgetMib, reservedMib + reserveMib, 1);
  const pct = budgetMib > 0 ? Math.round((reservedMib / budgetMib) * 100) : 0;

  const shown = jobs.filter((j) => j.mib > 0 && j.mib / total >= MERGE_BELOW);
  const tiny = jobs.filter((j) => j.mib > 0 && j.mib / total < MERGE_BELOW);
  const tinyMib = tiny.reduce((s, j) => s + j.mib, 0);
  const moreLabel = `${tiny.length} more`;

  const parts = [
    ...shown.map((j) => ({
      key: j.key,
      label: j.label,
      mib: j.mib,
      fill: 'fill-accent',
    })),
    ...(tiny.length > 0
      ? [{ key: 'more', label: moreLabel, mib: tinyMib, fill: 'fill-accent' }]
      : []),
    {
      key: 'free',
      label: 'Free to admit',
      mib: freeMib,
      fill: 'fill-surface-2',
    },
    { key: 'reserve', label: 'Reserve', mib: reserveMib, fill: 'fill-faint' },
  ].filter((p) => p.mib > 0);
  let at = 0;
  const placed = parts.map((p) => {
    const x = (at / total) * 100;
    at += p.mib;
    return { ...p, x, w: (p.mib / total) * 100 };
  });

  const valueText = `${gib(reservedMib)} of ${gib(budgetMib)} GiB reserved, ${gib(freeMib)} GiB free`;

  const legend = [
    ...shown.map((j) => ({
      key: j.key,
      label: j.label,
      mib: j.mib,
      swatch: 'bg-accent',
    })),
    ...(tiny.length > 0
      ? [{ key: 'more', label: moreLabel, mib: tinyMib, swatch: 'bg-accent' }]
      : []),
    {
      key: 'free',
      label: 'Free',
      mib: freeMib,
      swatch: 'bg-surface-2 border border-border-control',
    },
    { key: 'reserve', label: 'Reserve', mib: reserveMib, swatch: 'bg-faint' },
  ];

  return (
    <section className="flex w-full flex-col gap-md rounded-lg border border-border bg-surface px-xl py-lg">
      <div className="flex items-baseline justify-between gap-md">
        <h3 className="text-caption font-semibold text-muted">Memory budget</h3>
        <p className="text-small text-muted">
          <span className="font-mono tabular-nums">{gib(freeMib)} GiB</span>{' '}
          free to admit
          {safetyMargin !== undefined && (
            <>
              <Pipe />
              margin{' '}
              <span className="font-mono tabular-nums">
                {safetyMargin.toFixed(2)}
              </span>
            </>
          )}
        </p>
      </div>
      <div className="flex items-end justify-between gap-lg">
        <p className="flex items-baseline gap-sm">
          <span
            className={`font-mono text-display font-semibold tabular-nums ${over ? 'text-amber' : 'text-fg-strong'}`}
          >
            {gib(reservedMib)}
          </span>
          <span className="text-small text-muted">
            /{' '}
            <span className="font-mono tabular-nums">{gib(budgetMib)} GiB</span>{' '}
            reserved
            <Pipe />
            <span className="font-mono tabular-nums">{pct} %</span>
          </span>
        </p>
        <p className="flex items-baseline gap-lg text-small text-muted">
          <span>
            <span className="font-mono text-body font-semibold tabular-nums text-fg-strong">
              {admitted}
            </span>{' '}
            admitted
          </span>
          <span>
            <span
              className={`font-mono text-body font-semibold tabular-nums ${parked > 0 ? 'text-amber' : 'text-fg-strong'}`}
            >
              {parked}
            </span>
            <span className={parked > 0 ? 'text-amber' : undefined}>
              {' '}
              parked
            </span>
          </span>
        </p>
      </div>
      <Meter
        value={reservedMib}
        max={budgetMib}
        label="Memory budget"
        valueText={valueText}
      >
        <svg aria-hidden focusable={false} className="h-rail w-full">
          <defs>
            <mask id={mask}>
              {/* A mask is read by luminance: white shows, black cuts. Not theme colours. */}
              {/* eslint-disable-next-line design-tokens/no-raw-colour -- mask luminance */}
              <rect width="100%" height="100%" rx={2} fill="#fff" />
              {placed.slice(1).map((p) => (
                <rect
                  key={p.key}
                  x={`${p.x}%`}
                  width={GAP}
                  height="100%"
                  // eslint-disable-next-line design-tokens/no-raw-colour -- mask luminance
                  fill="#000"
                  transform={`translate(${-GAP / 2} 0)`}
                />
              ))}
            </mask>
          </defs>
          <g mask={`url(#${mask})`}>
            {placed.map((p) => (
              <rect
                key={p.key}
                x={`${p.x}%`}
                width={`${p.w}%`}
                height="100%"
                className={p.fill}
              >
                <title>{`${p.label}, ${gib(p.mib)} GiB`}</title>
              </rect>
            ))}
          </g>
        </svg>
      </Meter>
      <ul className="flex flex-wrap gap-x-lg gap-y-2xs text-caption text-muted">
        {legend.map((l) => (
          <li key={l.key} className="flex items-center gap-xs">
            <span
              aria-hidden
              className={`size-md shrink-0 rounded-xs ${l.swatch}`}
            />
            {l.label}{' '}
            <span className="font-mono tabular-nums text-faint">
              {gib(l.mib)} GiB
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}
