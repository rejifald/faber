import { ArrowRight } from 'lucide-react';
import { Icon } from './Icon';
import { focusRing } from './ladder';

export interface TextLinkProps {
  /** Where it goes. A path on this host or a URL. */
  href: string;
  /** The link's words. */
  children: string;
  /** Ends the link with an arrow, for a link that goes to another view. Default off. */
  arrow?: boolean;
  /** Called on a plain click, which then stays on this page (the link's own navigation is cancelled); a modified click still opens the `href`. Omit to let the link navigate. */
  onNavigate?: () => void;
}

/**
 * A plain link in the accent text colour, underlined on hover, for going to another view or page.
 * It is an anchor, so it opens in the same tab and can be copied; the page's shell serves each
 * view at its own path.
 *
 * @useWhen a screen points at another view or page: "All activity", "Open Memory". An action that
 * does something on this page is a Button with the `link` variant, not a TextLink.
 * @states default, hover, keyboard focus, with arrow, routes in place
 */
export function TextLink({ href, children, arrow = false, onNavigate }: TextLinkProps) {
  return (
    <a
      href={href}
      onClick={
        onNavigate
          ? (e) => {
              // A plain click routes in place; a modified one (new tab, new window) is the browser's.
              if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
              e.preventDefault();
              onNavigate();
            }
          : undefined
      }
      className={`inline-flex items-center gap-2xs rounded-xs text-small text-accent-text underline-offset-2 hover:underline ${focusRing}`}
    >
      {children}
      {arrow && <Icon icon={ArrowRight} size="sm" />}
    </a>
  );
}
