import { useState } from 'react';
import { DONE_FADE_MS, DONE_LINGER_MS, PIPS_MAX, type Pip, type PipKind } from '../popover/groups';
import { useReducedMotion } from '../popover/motion';

// Pip geometry (popover/anatomy.md § 5): a 9 px glyph whose shape carries the state, so a pip reads
// without its colour (colour-blind readers). Over six, a 56 × 6 bar of one segment per job. Drawn
// in SVG, whose own units carry the half pixels the spacing scale has no step for.
const PIP = 9;
const C = PIP / 2;
const RING_R = 3.6;
const BAR = { w: 56, h: 6, between: 1.5, r: 1.5 };
/**
 * Narrower than this, a segment per job stops reading: runs of one kind share a segment. At 4 px
 * an outlined segment (1 px stroke) keeps a 2 px hollow, so waiting never reads as frozen. One
 * per job holds up to 10 jobs.
 */
const SEGMENT_MIN = 4;
/** An outlined segment's stroke, drawn inside it. */
const OUTLINE = 1;
/** A running pip's arc turns once every 1.2 s, each pip 0.4 s behind the one before. */
const SPIN = { dur: 1.2, step: 0.4 };

// The marks keep their colours on every row state: hover and press are a neutral lift (#369).
const tone = {
  muted: 'stroke-muted',
  green: 'stroke-green',
  red: 'stroke-red',
  amber: 'stroke-amber',
  amberFill: 'fill-amber',
  faint: 'stroke-faint',
};

/**
 * A done pip's fade: full for 20 s after its verdict arrived, then to nothing over the last 10 s
 * (anatomy § 5, Motion). Read once, when the pip first draws: an SVG's animation clock starts
 * with the element, so the offset into the 30 s is fixed then, and the pip's stable key keeps
 * it from restarting on the next snapshot.
 */
function Fade({ ageMs }: { ageMs: number }) {
  const [at] = useState(() => {
    const age = Math.max(0, ageMs);
    const hold = DONE_LINGER_MS - DONE_FADE_MS;
    const left = Math.max(0, DONE_LINGER_MS - age);
    // Already inside the fade: start from where it would be, and fade what is left.
    const from = age <= hold ? 1 : left / DONE_FADE_MS;
    return {
      begin: Math.max(0, hold - age) / 1000,
      dur: Math.max(0.001, Math.min(left, DONE_FADE_MS)) / 1000,
      from,
    };
  });
  return (
    <animate attributeName="opacity" values={`${at.from};0`} begin={`${at.begin}s`} dur={`${at.dur}s`} fill="freeze" />
  );
}

/**
 * The six glyphs: ✓ done (muted: CI passing or failing is the job's business, not faber's),
 * slashed ring runner failed, turning arc running, pause frozen, clock waiting, and a grey ring
 * for a peer's idle runner.
 */
function Glyph({ kind, phase, still }: { kind: PipKind; phase: number; still: boolean }) {
  switch (kind) {
    case 'done':
      return (
        <path
          d="M1.5 4.8l2 2 4-4.6"
          fill="none"
          strokeWidth={1.6}
          strokeLinecap="round"
          strokeLinejoin="round"
          className={tone.muted}
        />
      );
    case 'fault':
      return (
        <>
          <circle cx={C} cy={C} r={RING_R} fill="none" strokeWidth={1.3} className={tone.red} />
          <path d="M2 7L7 2" strokeWidth={1.3} strokeLinecap="round" className={tone.red} />
        </>
      );
    case 'running':
      return (
        <>
          <circle cx={C} cy={C} r={RING_R} fill="none" strokeWidth={1.6} opacity={0.25} className={tone.green} />
          <path
            d={`M${C} ${C - RING_R}a${RING_R} ${RING_R} 0 0 1 ${RING_R} ${RING_R}`}
            fill="none"
            strokeWidth={1.6}
            strokeLinecap="round"
            className={tone.green}
          >
            {!still && (
              // Keyed by phase, so SMIL restarts with the new offset when the pip's place changes.
              <animateTransform
                key={`spin-${phase}`}
                attributeName="transform"
                type="rotate"
                from={`0 ${C} ${C}`}
                to={`360 ${C} ${C}`}
                dur={`${SPIN.dur}s`}
                begin={`${-phase * SPIN.step}s`}
                repeatCount="indefinite"
              />
            )}
          </path>
        </>
      );
    case 'frozen':
      return (
        <>
          {/* Whole pixels: at 1x a bar on a half pixel blurs into a smear. */}
          <rect x={2} y={1} width={2} height={7} rx={1} className={tone.amberFill} />
          <rect x={5} y={1} width={2} height={7} rx={1} className={tone.amberFill} />
        </>
      );
    case 'idle':
      // The running pip's ring with no arc, grey at full strength so it keeps 3:1 (WCAG 1.4.11):
      // a runner ready for a job. When it takes one, the arc appears in the same ring.
      return <circle cx={C} cy={C} r={RING_R} fill="none" strokeWidth={1.6} className={tone.faint} />;
    case 'waiting':
      return (
        <>
          <circle cx={C} cy={C} r={RING_R} fill="none" strokeWidth={1.3} className={tone.amber} />
          <path d="M4.5 2.5v2h1.5" fill="none" strokeWidth={1} strokeLinecap="round" className={tone.amber} />
        </>
      );
  }
}

function Mark({ kind, phase, still, ageMs }: { kind: PipKind; phase: number; still: boolean; ageMs?: number }) {
  return (
    <svg width={PIP} height={PIP} viewBox={`0 0 ${PIP} ${PIP}`} aria-hidden className="shrink-0">
      <g>
        {kind === 'done' && !still && ageMs !== undefined && <Fade ageMs={ageMs} />}
        <Glyph kind={kind} phase={phase} still={still} />
      </g>
    </svg>
  );
}

// A segment per job in the overflow bar, at full strength so every one holds 3:1 against the row
// (WCAG 1.4.11). Live work is filled; done, faber's fault and waiting are outlines (anatomy § 5,
// States).
interface Segment {
  fill: boolean;
  className: string;
}

const SEGMENT: Record<PipKind, Segment> = {
  done: { fill: false, className: tone.muted },
  fault: { fill: false, className: tone.red },
  running: { fill: true, className: 'fill-green' },
  frozen: { fill: true, className: tone.amberFill },
  waiting: { fill: false, className: tone.amber },
  idle: { fill: false, className: tone.faint },
};

/** The bar's rules as the tests read them: the segment floor, the outline stroke and each kind's look. */
export const barGeometry = { segmentMin: SEGMENT_MIN, outline: OUTLINE, segments: SEGMENT };

/**
 * The segments' kinds and widths: one per job while each is at least SEGMENT_MIN wide, else one
 * per run of a kind (the kinds arrive grouped, so at most six), sized by its share of the jobs.
 */
export function barSegments(kinds: PipKind[]): { kind: PipKind; x: number; w: number }[] {
  const perJob = (BAR.w - BAR.between * (kinds.length - 1)) / kinds.length;
  if (perJob >= SEGMENT_MIN)
    return kinds.map((kind, i) => ({
      kind,
      x: i * (perJob + BAR.between),
      w: perJob,
    }));
  const runs: { kind: PipKind; n: number }[] = [];
  for (const kind of kinds) {
    const last = runs.at(-1);
    if (last?.kind === kind) last.n++;
    else runs.push({ kind, n: 1 });
  }
  // Runs too small for their share get SEGMENT_MIN; the rest of the room is shared by job count.
  const room = BAR.w - BAR.between * (runs.length - 1);
  const pinned = new Set<number>();
  let widths: number[] = [];
  for (;;) {
    const free = room - pinned.size * SEGMENT_MIN;
    const jobs = runs.reduce((a, r, k) => (pinned.has(k) ? a : a + r.n), 0);
    widths = runs.map((r, k) => (pinned.has(k) ? SEGMENT_MIN : (free * r.n) / jobs));
    const short = widths.findIndex((w, k) => !pinned.has(k) && w < SEGMENT_MIN);
    if (short < 0) break;
    pinned.add(short);
  }
  return runs.map((r, i) => ({
    kind: r.kind,
    x: widths.slice(0, i).reduce((a, b) => a + b + BAR.between, 0),
    w: widths[i],
  }));
}

/** The overflow bar: one segment per mark (or per run of a kind), in strip order: a 56 × 6 band. */
function Bar({ kinds }: { kinds: PipKind[] }) {
  return (
    <span aria-hidden className="flex shrink-0 items-center gap-xs">
      <span className="font-mono text-caption text-muted">{kinds.length}</span>
      <svg width={BAR.w} height={BAR.h} viewBox={`0 0 ${BAR.w} ${BAR.h}`} aria-hidden className="shrink-0">
        {barSegments(kinds).map(({ kind, x, w: width }, i) => {
          const s = SEGMENT[kind];
          // Edges snapped to whole pixels, so at 1x an outline is one crisp pixel, not two faint
          // ones. A whole-pixel floor keeps its width; a gap becomes 1 or 2 px.
          const at = Math.round(x);
          const w = Math.round(x + width) - at;
          // An outline is drawn inside the segment: inset by half its stroke.
          return s.fill ? (
            <rect key={i} x={at} y={0} width={w} height={BAR.h} rx={BAR.r} className={s.className} />
          ) : (
            <rect
              key={i}
              x={at + OUTLINE / 2}
              y={OUTLINE / 2}
              width={w - OUTLINE}
              height={BAR.h - OUTLINE}
              rx={BAR.r - OUTLINE / 2}
              fill="none"
              strokeWidth={OUTLINE}
              className={s.className}
            />
          );
        })}
      </svg>
    </span>
  );
}

export interface PipStripProps {
  /** One per job, already ordered: finished, then running, frozen, waiting. Over six, drawn as a bar. */
  pips: Pip[];
  /** Holds every pip still whatever the system says; by default the reader's reduced-motion setting decides. */
  still?: boolean;
}

/**
/**
 * One glyph per job in a group (or per runner on a peer row), the shape carrying the state: done ✓
 * (muted, fading out over its 30 s, whether CI passed or failed), runner failed (slashed red
 * ring), running (a turning green arc), frozen (amber pause), waiting (amber clock) and idle (a
 * grey ring, a peer's runner with no job). Over six, the count and a bar in the same order: one
 * segment per mark up to 10, then one per run of a kind; running and frozen filled, done, fault,
 * waiting and idle outlined.
 * Decorative: the row's accessible name says the same in words. Under reduced motion the arc
 * holds still and nothing fades.
 *
 * @useWhen the job group row of the popover, one glyph on a folded group's compact line, and a
 * peer row's runners (running per busy runner, then `idle`, a grey ring, per idle one).
 * @states done, running, frozen, waiting, fault, idle, mixed, overflow bar, overflow with idle, overflow with a fault, on a hovered row, overflow on a hovered row, many jobs, short outlined runs, one outlined job in a long bar, reduced motion
 */
export function PipStrip({ pips, still: forceStill }: PipStripProps) {
  const reduced = useReducedMotion();
  const still = forceStill ?? reduced;
  if (pips.length > PIPS_MAX) {
    return <Bar kinds={pips.map((p) => p.kind)} />;
  }
  let phase = 0;
  return (
    <span aria-hidden className="flex shrink-0 items-center gap-2xs">
      {pips.map((p) => (
        // Keyed by `still` too: an SVG's animation clock starts with the element, so a fade
        // added later must come with a fresh element, or its offset counts from the old one.
        <Mark
          key={`${p.key}-${still}`}
          kind={p.kind}
          phase={p.kind === 'running' ? phase++ : 0}
          still={still}
          ageMs={p.ageMs}
        />
      ))}
    </span>
  );
}
