import { useImperativeHandle, useLayoutEffect, useRef, useState, type ReactNode, type Ref } from 'react';

export interface ScrollAreaProps {
  /** What scrolls. Lay it out inside: the area only clips and scrolls it. */
  children: ReactNode;
  /** The tallest the area grows, in px; past it the content scrolls. Unset, it grows with its content. */
  maxHeight?: number;
  /** Draws a 24 px fade into the card at an edge that has more content beyond it. */
  fades?: boolean;
  /** The id of the scrolling element, for `aria-controls`. */
  id?: string;
  /** The scrolling element, for a screen that measures it. */
  ref?: Ref<HTMLDivElement>;
}

/**
 * A vertical scroll container that applies its height cap itself, so a screen passes a measured
 * number and never a `style`. With `fades`, the top edge fades once scrolled and the bottom edge
 * while more lies below, from the card's own background into the content.
 *
 * @useWhen a list that must stop growing at a height the screen measured: the popover's open
 * jobs column, and its attention rows past the window's cap. For a list that never scrolls, no wrapper.
 * @states fits, scrolls, scrolled with fades
 */
export function ScrollArea({ children, maxHeight, fades = false, id, ref }: ScrollAreaProps) {
  const box = useRef<HTMLDivElement>(null);
  useImperativeHandle(ref, () => box.current as HTMLDivElement);
  const [edge, setEdge] = useState({ top: false, bottom: false });
  const read = () => {
    const n = box.current;
    if (!n || !fades) return;
    const top = n.scrollTop > 0;
    const bottom = n.scrollTop + n.clientHeight < n.scrollHeight - 1;
    setEdge((e) => (e.top === top && e.bottom === bottom ? e : { top, bottom }));
  };
  // After every render: the content or the cap may have changed what lies beyond an edge.
  useLayoutEffect(read);
  return (
    <div className="relative">
      <div
        ref={box}
        id={id}
        onScroll={read}
        style={maxHeight === undefined ? undefined : { maxHeight }}
        className="flex flex-col overflow-y-auto"
      >
        {children}
      </div>
      {fades && edge.top && <div aria-hidden className="pointer-events-none absolute inset-x-0 top-0 h-3xl bg-linear-to-b from-bg to-transparent" />}
      {fades && edge.bottom && (
        <div aria-hidden className="pointer-events-none absolute inset-x-0 bottom-0 h-3xl bg-linear-to-t from-bg to-transparent" />
      )}
    </div>
  );
}
