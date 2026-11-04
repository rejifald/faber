import type { LucideIcon } from 'lucide-react';
import { cx, iconSize, type Size } from './ladder';

export interface IconProps {
  /** The lucide icon. */
  icon: LucideIcon;
  /** The ladder step of the text it sits beside (ADR-0040 § 3): `sm` 12, `md` 14, `lg` 16. */
  size?: Size;
  /** Nudges it down to the first line's centre when it leads a block of wrapping text. */
  leading?: boolean;
}

/**
 * A lucide icon outside a control, sized by the ladder step its text matches, in the text's
 * colour. Always decorative to assistive tech: the words beside it, or the row's name, carry the meaning.
 *
 * Or a status glyph whose shape carries a state, in a row whose accessible name says that state
 * in words (a VM row's icon).
 *
 * @useWhen an icon that leads a line of text: an attention row's severity mark, a section's glyph,
 * a VM row's status.
 * Inside a control, the control draws its own icon; an icon that does something is an IconButton.
 * @states default
 */
export function Icon({ icon: Glyph, size = 'md', leading = false }: IconProps) {
  return <Glyph aria-hidden focusable={false} className={cx('shrink-0', iconSize[size], leading && 'mt-3xs')} />;
}
