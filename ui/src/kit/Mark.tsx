import { useId } from 'react';

import { cx, iconSize, type Size } from './ladder';

export interface MarkProps {
  /** `logo`: the mark, in the accent. `offline`: the mark faded and struck through, in the text's colour. */
  variant?: 'logo' | 'offline';
  /** The ladder step it is sized by (ADR-0040 § 3). */
  size?: Size;
}

/**
 * The mark's faces on its 100-unit grid (`design/mark/`): a cube in 2:1 isometric, edge 29, centred
 * on (50,50), whose front-corner core is cut free of the shell by a 3-unit moat. Each face is
 * [points, opacity], top / left / right; the core carries the weight. Lit from the side (top in
 * the middle step), so the cube reads the right way up in one colour on light and dark alike.
 */
const SHELL: [string, number][] = [
  ['50,21 79,35.5 67.854,41.073 50,32.146 32.146,41.073 21,35.5', 0.38],
  ['21,64.5 50,79 50,67.854 32.5,59.104 32.5,41.25 21,35.5', 0.55],
  ['79,64.5 50,79 50,67.854 67.5,59.104 67.5,41.25 79,35.5', 0.24],
];
const CORE: [string, number][] = [
  ['50,35.5 64.5,42.75 50,50 35.5,42.75', 0.78],
  ['35.5,57.25 50,64.5 50,50 35.5,42.75', 1],
  ['64.5,57.25 50,64.5 50,50 64.5,42.75', 0.56],
];
const OFFLINE_FADE = 0.5;
const SLASH_WIDTH = 6.5;
/**
 * The faded mark is clipped to the two triangles either side of the slash, leaving a 14-unit
 * channel along the diagonal, so the slash reads as cutting through the cube.
 */
const OUTSIDE_SLASH = ['29.9,20 80,20 80,70.1', '20,29.9 20,80 70.1,80'];

function MarkShapes() {
  return (
    <>
      {[...SHELL, ...CORE].map(([points, opacity]) => (
        <polygon key={points} points={points} fillOpacity={opacity} />
      ))}
    </>
  );
}

/**
 * faber's own marks (ADR-0040 § 2, ADR-0042 § 5): the logo (a cube with its front-corner core cut free)
 * and the offline glyph (the logo faded and struck through). Drawn from the app icon's geometry in
 * `design/mark/`, filled rather than stroked, so they sit off Lucide's grid. Decorative: the brand
 * name or the offline title beside it says the same.
 *
 * @useWhen the surface's header (logo) and the daemon-offline badge (offline). Never as a status
 * dot or a button icon.
 * @states logo, offline
 */
export function Mark({ variant = 'logo', size = 'lg' }: MarkProps) {
  const clipId = `mark-clip-${useId().replace(/[^a-zA-Z0-9_-]/g, '')}`;
  return (
    <svg
      viewBox="20 20 60 60"
      aria-hidden
      focusable={false}
      fill="currentColor"
      className={cx('shrink-0', iconSize[size], variant === 'logo' && 'text-accent')}
    >
      {variant === 'logo' ? (
        <MarkShapes />
      ) : (
        <>
          <clipPath id={clipId}>
            {OUTSIDE_SLASH.map((points) => (
              <polygon key={points} points={points} />
            ))}
          </clipPath>
          <g opacity={OFFLINE_FADE} clipPath={`url(#${clipId})`}>
            <MarkShapes />
          </g>
          <line x1="28" y1="28" x2="72" y2="72" stroke="currentColor" strokeWidth={SLASH_WIDTH} strokeLinecap="round" />
        </>
      )}
    </svg>
  );
}
