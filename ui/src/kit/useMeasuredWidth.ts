import { useEffect, useRef, useState, type RefObject } from 'react';

/**
 * The content width of the element `ref` points at, in CSS pixels, kept current as it resizes.
 * `fallback` is the width before the first measurement and wherever there is no layout (server
 * render, tests), so a chart can draw at its own scale without a viewBox stretching its text.
 */
export function useMeasuredWidth<T extends HTMLElement>(fallback: number): [RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(fallback);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const read = () => {
      const w = Math.floor(el.getBoundingClientRect().width);
      if (w > 0) setWidth(w);
    };
    read();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(read);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, width];
}
