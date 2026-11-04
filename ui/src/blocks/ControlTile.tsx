import type { ReactNode } from 'react';
import { StatusDot, type StatusTone } from '../kit';

export interface ControlTileProps {
  /** What the tile controls, in sentence case: "Host". */
  label: string;
  /** The state as one word or short phrase, the tile's hero: "Running". */
  state: ReactNode;
  /** Calm by default: neutral (or omitted) is the normal text colour with a quiet dot, for a healthy state. Colour only what needs attention: amber held or paused, red failed, accent an offer. */
  tone?: StatusTone;
  /** One or two lines under the state: what it means, or why. */
  note?: ReactNode;
  /** Why the last action failed, read out as a status in red above the actions. */
  error?: string;
  /** The tile's buttons, at the bottom. Leave them out when no action is on offer. */
  children?: ReactNode;
}

const STATE_TONE: Record<StatusTone, string> = {
  green: 'text-green',
  amber: 'text-amber',
  red: 'text-red',
  accent: 'text-accent-text',
  neutral: 'text-fg-strong',
};

/**
 * One controllable thing: its label, its state as the hero, a note, and the actions that change it.
 *
 * @useWhen a dashboard row of controls, each one thing with its state and its action, such as Host
 * or Docker admission on Maintenance. Tiles in a row share a height and sit their actions on the
 * bottom edge. Not for a headline figure; use StatTile. Not for a list of rows; use Panel.
 * @states calm (running, open, up to date, N running), paused or held (amber), failed (red), update on offer (accent), with note, without note, busy action, action failed, no action
 */
export function ControlTile({ label, state, tone = 'neutral', note, error, children }: ControlTileProps) {
  return (
    <div className="flex h-full min-w-0 flex-col gap-xs rounded-lg border border-border bg-surface px-xl py-lg">
      <div className="text-caption font-semibold text-muted">{label}</div>
      <div className={`flex items-center gap-sm text-title font-semibold ${STATE_TONE[tone]}`}>
        <StatusDot tone={tone} />
        <span className="min-w-0 wrap-anywhere">{state}</span>
      </div>
      {note && <div className="text-caption text-muted wrap-anywhere">{note}</div>}
      <p role="status" className="text-caption text-red empty:hidden">
        {error}
      </p>
      {children && <div className="mt-auto flex flex-wrap items-center gap-sm pt-md">{children}</div>}
    </div>
  );
}
