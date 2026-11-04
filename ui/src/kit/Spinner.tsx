import { LoaderCircle } from 'lucide-react';
import { iconSize, type Size } from './ladder';

/** The spinner a loading control shows in place of its icon, sized by the control's step. */
export function spinner(size: Size) {
  return <LoaderCircle aria-hidden className={`${iconSize[size]} motion-safe:animate-spin`} />;
}
