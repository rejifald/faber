import { createContext, useContext, useSyncExternalStore } from 'react';

const QUERY = '(prefers-reduced-motion: reduce)';

const subscribe = (onChange: () => void) => {
  if (typeof window === 'undefined' || !window.matchMedia) return () => {};
  const mq = window.matchMedia(QUERY);
  mq.addEventListener('change', onChange);
  return () => mq.removeEventListener('change', onChange);
};
const read = () => (typeof window !== 'undefined' && window.matchMedia ? window.matchMedia(QUERY).matches : false);

/**
 * Holds every pip still beneath it, whatever the system says. Only the story browser sets it
 * (`parameters.still`): a story is one snapshot that never refreshes, so a passed pip's fade
 * would run out and leave a row the app never shows, where the app's next snapshot drops it.
 */
export const StillMotion = createContext(false);

/**
 * Whether the reader asked for less motion. The running pip's turning arc is SVG animation, which CSS's
 * `prefers-reduced-motion` cannot reach, so the strip reads the setting and leaves it out.
 */
export function useReducedMotion(): boolean {
  const still = useContext(StillMotion);
  const reduced = useSyncExternalStore(subscribe, read, () => false);
  return still || reduced;
}
