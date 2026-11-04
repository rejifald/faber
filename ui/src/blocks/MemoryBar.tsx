import { useId } from 'react';
import { gib } from '../popover/limits';

export interface MemoryBarProps {
  /** Engine memory in MiB: the budget plus the reserve. */
  totalMib: number;
  /** Kept for the VM itself, MiB. */
  reserveMib: number;
  /** Reserved by running jobs, MiB. */
  jobsMib: number;
  /** Left free between jobs, MiB. */
  headroomMib: number;
}

/** Width of the gap between segments and of the headroom's left edge, in px (SVG user units). */
const GAP = 2;

/**
 * How the engine's memory divides now: reserve, jobs, free budget and headroom, as one bar of four
 * segments with a legend of the numbers in GiB. Free is what is left; over budget it is absent.
 *
 * @useWhen the Limits strip shows where the engine's memory goes. For one fraction of a limit use GaugeBar.
 * @states default, no jobs (Jobs segment absent, legend reads 0.0), over budget (Free segment absent)
 */
export function MemoryBar({ totalMib, reserveMib, jobsMib, headroomMib }: MemoryBarProps) {
  const freeMib = Math.max(0, totalMib - reserveMib - jobsMib - headroomMib);
  const sum = Math.max(totalMib, reserveMib + jobsMib + freeMib + headroomMib, 1);
  const mask = useId();
  const parts = [
    { key: 'reserve', mib: reserveMib, fill: 'fill-faint' },
    { key: 'jobs', mib: jobsMib, fill: 'fill-green' },
    { key: 'free', mib: freeMib, fill: 'fill-green-soft' },
    { key: 'headroom', mib: headroomMib, fill: 'fill-amber-soft' },
  ].filter((p) => p.mib > 0);
  let at = 0;
  const placed = parts.map((p) => {
    const x = (at / sum) * 100;
    at += p.mib;
    return { ...p, x, w: (p.mib / sum) * 100 };
  });
  const headroom = placed.find((p) => p.key === 'headroom');
  const legend = [
    { label: 'Reserve', mib: reserveMib, swatch: 'bg-faint' },
    { label: 'Jobs', mib: jobsMib, swatch: 'bg-green' },
    { label: 'Free budget', mib: freeMib, swatch: 'bg-green-soft border border-border-control' },
    { label: 'Headroom', mib: headroomMib, swatch: 'bg-amber' },
  ];
  return (
    <section className="flex flex-col gap-sm p-lg">
      <h3 className="flex items-baseline justify-between text-body font-semibold text-fg-strong">
        Engine memory
        <span className="font-mono text-small font-normal text-muted">{gib(totalMib)} GiB</span>
      </h3>
      <svg aria-hidden focusable={false} className="h-bar w-full">
        <defs>
          <mask id={mask}>
            {/* A mask is read by luminance: white shows, black cuts. Not theme colours. */}
            {/* eslint-disable-next-line design-tokens/no-raw-colour -- mask luminance */}
            <rect width="100%" height="100%" rx={2} fill="#fff" />
            {placed.slice(1).map((p) => (
              // eslint-disable-next-line design-tokens/no-raw-colour -- mask luminance
              <rect key={p.key} x={`${p.x}%`} width={GAP} height="100%" fill="#000" transform={`translate(${-GAP / 2} 0)`} />
            ))}
          </mask>
        </defs>
        <g mask={`url(#${mask})`}>
          {placed.map((p) => (
            <rect key={p.key} x={`${p.x}%`} width={`${p.w}%`} height="100%" className={p.fill} />
          ))}
          {headroom && <rect x={`${headroom.x}%`} width={GAP} height="100%" className="fill-amber" transform={`translate(${GAP / 2} 0)`} />}
        </g>
      </svg>
      <ul className="grid grid-cols-2 gap-x-lg gap-y-2xs text-caption text-muted">
        {legend.map((l) => (
          <li key={l.label} className="flex items-center gap-xs">
            <span aria-hidden className={`size-md shrink-0 ${l.swatch}`} />
            {l.label} {gib(l.mib)}
          </li>
        ))}
      </ul>
    </section>
  );
}
