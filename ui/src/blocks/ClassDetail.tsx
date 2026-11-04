import { X } from 'lucide-react';
import { useState } from 'react';
import { Button, Field, IconButton, Input, PeakHistogram, RunStrip, StatusBadge } from '../kit';
import { gibText, idleLine, peakText, pinRequest, type ClassView } from '../dashboard/memory';
import { fmtAgo } from '../popover/format';
import { StatTile } from './StatTile';

export interface ClassDetailProps {
  /** The class to show, as `classViews` shapes it. */
  cls: ClassView;
  /** Closes the panel; Memory clears its selection. */
  onClose: () => void;
  /** Saves a pin of these caps, in MiB. */
  onPin: (runnerMib: number, dindMib: number) => void;
  /** Returns a pinned class to learned; offered only while the class is pinned. */
  onUnpin?: () => void;
  /** A save is in flight: Save pin shows its spinner and the fields hold. */
  pending?: boolean;
  /** The daemon's own text when the last save failed. */
  error?: string;
}

const Heading = ({ children }: { children: string }) => <h3 className="text-caption font-semibold text-muted">{children}</h3>;

/**
 * Memory's side panel for the selected class: its caps and measured peak, why the cap is what it
 * is, how its peaks spread and how the last runs went, and a form to pin its caps.
 *
 * @useWhen Memory has a class selected. Not a general detail view: it reads one class's view model.
 * @states learned, pinned, warned, bimodal, still learning, saving a pin, pin failed
 */
export function ClassDetail({ cls, onClose, onPin, onUnpin, pending = false, error }: ClassDetailProps) {
  const [runner, setRunner] = useState(gibText(cls.runnerMib));
  const [dind, setDind] = useState(gibText(cls.dindMib));
  const [invalid, setInvalid] = useState(false);
  const suggest = (cls.bimodal || cls.warned) && cls.coldSafe ? cls.coldSafe : null;
  const cold = cls.bimodal;
  const shown = cls.runnerPeaks.length;

  const save = () => {
    const req = pinRequest(cls.key, runner, dind);
    if (!req || req.runner_mib === undefined || req.dind_mib === undefined) {
      setInvalid(true);
      return;
    }
    setInvalid(false);
    onPin(req.runner_mib, req.dind_mib);
  };
  const reset = () => {
    setRunner(gibText(cls.runnerMib));
    setDind(gibText(cls.dindMib));
    setInvalid(false);
  };

  return (
    <section aria-label={`Class ${cls.name}`} className="flex min-w-0 flex-col gap-xl rounded-lg border border-border bg-surface px-xl py-lg">
      <div className="flex items-start justify-between gap-md">
        <h2 className="min-w-0 font-mono text-title font-semibold wrap-anywhere text-fg-strong">{cls.name}</h2>
        <IconButton icon={X} label="Close class details" size="sm" onClick={onClose} />
      </div>
      <div className="flex flex-wrap items-center gap-sm text-caption text-muted">
        {cls.statusLabel && <StatusBadge tone={cls.statusTone}>{cls.statusLabel}</StatusBadge>}
        {idleLine(cls)}
      </div>

      <div className="grid grid-cols-3 gap-md">
        <StatTile size="sm" label="Cap in force" value={`${gibText(cls.capMib)}G`} note={`${gibText(cls.runnerMib)} runner + ${gibText(cls.dindMib)} dind`} />
        {cold && cls.peakMib !== null && cls.warmPeakMib !== null ? (
          <StatTile
            size="sm"
            label="Measured peak"
            tone="red"
            value={
              <span className="flex flex-col">
                <span className="text-fg-strong">
                  {gibText(cls.warmPeakMib)} <span className="text-caption font-normal text-muted">warm</span>
                </span>
                <span>
                  {gibText(cls.peakMib)} <span className="text-caption font-normal text-muted">cold</span>
                </span>
              </span>
            }
            note={`${cls.coldRuns} of ${shown} runs went cold`}
          />
        ) : (
          <StatTile
            size="sm"
            label="Measured peak"
            tone={cls.overCap ? 'red' : 'neutral'}
            value={peakText(cls)}
            note={cls.peakMib === null ? 'No samples yet' : `highest of ${cls.samples} runs`}
          />
        )}
        <StatTile
          size="sm"
          label="Pinned override"
          value={cls.pinned ? `${gibText(cls.capMib)}G` : '—'}
          note={cls.pinned ? 'set by a person' : 'Not pinned: set one below'}
        />
      </div>

      {cls.explanation.length > 0 && (
        <div className="flex flex-col gap-sm rounded-md bg-surface-2 px-lg py-md text-small text-fg">
          {cls.explanation.map((p) => (
            <p key={p}>{p}</p>
          ))}
        </div>
      )}

      <div className="flex flex-col gap-sm">
        <Heading>{`Peak distribution, ${shown} samples`}</Heading>
        {shown > 0 ? (
          <PeakHistogram peaks={cls.runnerPeaks} capMib={cls.runnerMib} safeMib={suggest?.runnerMib} coldFromMib={cls.coldFromMib ?? undefined} />
        ) : (
          <p className="text-small text-muted">No peaks recorded yet.</p>
        )}
      </div>

      <div className="flex flex-col gap-sm">
        <Heading>{cls.recentPeaks.length ? `Last ${cls.recentPeaks.length} runs` : 'Last runs'}</Heading>
        {cls.recentPeaks.length > 0 ? (
          <RunStrip peaks={cls.recentPeaks} capMib={cls.runnerMib} censored={cls.recentCensored} endLabel={cls.lastRanAgeS === null ? undefined : `${fmtAgo(cls.lastRanAgeS)} ago`} />
        ) : (
          <p className="text-small text-muted">No runs yet.</p>
        )}
      </div>

      <div className="flex flex-col gap-md border-t border-border-soft pt-lg">
        <Heading>Set a pin</Heading>
        <div className="grid grid-cols-2 gap-md">
          <Field label="Runner GiB" disabled={pending}>
            <Input type="number" min={0} step={0.1} mono value={runner} onValueChange={setRunner} />
          </Field>
          <Field label="Dind GiB" disabled={pending}>
            <Input type="number" min={0} step={0.1} mono value={dind} onValueChange={setDind} />
          </Field>
        </div>
        {suggest && (
          <div>
            <Button
              size="sm"
              disabled={pending}
              onClick={() => {
                setRunner(gibText(suggest.runnerMib));
                setDind(gibText(suggest.dindMib));
                setInvalid(false);
              }}
            >
              {`Use cold-safe value: ${gibText(suggest.runnerMib)} runner / ${gibText(suggest.dindMib)} dind`}
            </Button>
          </div>
        )}
        {(invalid || error) && (
          <p role="alert" className="text-small text-red">
            {invalid ? 'Enter the runner cap above zero and the dind cap, in GiB.' : error}
          </p>
        )}
        <div className="flex items-center gap-sm">
          <Button variant="primary" loading={pending} onClick={save}>
            Save pin
          </Button>
          <Button variant="link" disabled={pending} onClick={reset}>
            Cancel
          </Button>
          {cls.pinned && onUnpin && (
            <Button variant="link" disabled={pending} onClick={onUnpin}>
              Return to learned
            </Button>
          )}
        </div>
      </div>
    </section>
  );
}
