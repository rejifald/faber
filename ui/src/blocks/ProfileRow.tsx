import { ChevronRight } from 'lucide-react';
import { Fragment } from 'react';
import { Button, Icon } from '../kit';
import type { Config } from '../settings/types';
import { Pipe } from './Pipe';

/** The facts a profile row shows at the right: backend, Docker access, and a VM's cores and memory. */
export function profileFacts(profile: Config): string[] {
  const backend = String(profile.backend ?? '');
  const facts = backend ? [backend] : [];
  // A blank `dind` grants it, so only an explicit false is no access.
  if (backend === 'docker' && profile.dind !== false) facts.push('Docker access');
  if (backend === 'macos-vm') {
    if (typeof profile.cpus === 'number') facts.push(`${profile.cpus} vCPU`);
    if (typeof profile.memory_mib === 'number') facts.push(`${Number((profile.memory_mib / 1024).toFixed(1))} GiB`);
  }
  return facts;
}

export interface ProfileRowProps {
  /** Its place in the list, from 1: the order is routing. */
  order: number;
  /** The profile's name. */
  name: string;
  /** The runs-on labels it takes, in order. */
  labels: string[];
  /** What shows at the right, from `profileFacts`. */
  facts: string[];
  /** Opens the profile. */
  onOpen: () => void;
}

/**
 * One profile in the Profiles list: its order number, name and labels, its backend and Docker
 * access, and a chevron. The whole row opens it.
 *
 * @useWhen the list of profiles, one row per profile in routing order.
 * @states default, hover, focus-visible
 */
export function ProfileRow({ order, name, labels, facts, onOpen }: ProfileRowProps) {
  const body = (
    <>
      <span className="w-xl shrink-0 text-left font-mono text-caption font-normal text-muted">{order}</span>
      <span className="flex min-w-0 flex-1 flex-col text-left">
        <span className="text-body font-semibold text-fg-strong wrap-anywhere">{name}</span>
        <span className="font-mono text-caption font-normal text-muted wrap-anywhere">{labels.join(', ')}</span>
      </span>
    </>
  );
  // The whole row is one button (guidelines § Button variants, `row`), so the keyboard reaches it.
  return (
    <div className="grid border-b border-border-soft py-2xs">
      <Button variant="row" onClick={onOpen}>
        <span className="flex w-full items-center gap-lg py-xs">
          {body}
          <span className="text-small font-normal text-muted">
            {facts.map((f, i) => (
              <Fragment key={f}>
                {i > 0 && <Pipe />}
                {f}
              </Fragment>
            ))}
          </span>
          <Icon icon={ChevronRight} size="sm" />
        </span>
      </Button>
    </div>
  );
}
