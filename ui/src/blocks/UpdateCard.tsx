import { Button, TextLink } from '../kit';

export interface UpdateCardProps {
  /** The version that is ready ("0.3.5"). */
  version: string;
  /** When it installs and what it costs, in a phrase ("Installs when this host goes idle."). */
  body: string;
  /** Where "Details" goes (the Updates panel); omit for no link. */
  href?: string;
  /** Called by a plain click on Details, so the screen can route in place. */
  onDetails?: () => void;
  /** Called by Install now. */
  onInstall: () => void;
  /** Called by Skip; omit when the update cannot be skipped now and the button is not drawn. */
  onSkip?: () => void;
  /** The action in flight: its button shows a spinner and the other is disabled. */
  pending?: 'install' | 'skip';
  /** Why the last action failed, set as text under the body; cleared by the screen on the next try. */
  error?: string;
}

/**
 * A pending faber update at the foot of the sidebar: "0.3.5 is ready", when it installs, and
 * Install now, Skip when allowed, and a Details link to the Updates panel. Data arrives as props;
 * the screen runs the actions.
 *
 * @useWhen the sidebar foot, only while an update is pending. Nothing renders it when none is.
 * @states default, installing, failed, no skip, with details link
 */
export function UpdateCard({ version, body, href, onDetails, onInstall, onSkip, pending, error }: UpdateCardProps) {
  return (
    <section aria-label="Update" className="flex flex-col gap-xs rounded-md border border-border bg-surface p-md">
      <h2 className="text-small font-semibold text-accent-text">{`${version} is ready`}</h2>
      <p className="text-caption text-muted">{body}</p>
      {error && (
        <p role="alert" className="text-caption text-red">
          {error}
        </p>
      )}
      <div className="flex items-center gap-sm">
        <Button size="sm" variant="primary" loading={pending === 'install'} disabled={pending === 'skip'} onClick={onInstall}>
          Install now
        </Button>
        {onSkip && (
          <Button size="sm" variant="link" loading={pending === 'skip'} disabled={pending === 'install'} onClick={onSkip}>
            Skip
          </Button>
        )}
        {href && (
          <span className="ml-auto">
            <TextLink href={href} onNavigate={onDetails}>
              Details
            </TextLink>
          </span>
        )}
      </div>
    </section>
  );
}
