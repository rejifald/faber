import { Component, useCallback, useEffect, useState, type ErrorInfo, type ReactNode } from 'react';
import { rememberLive } from '../popover/groups';
import type { Inflight, Invoke, Snapshot } from '../popover/types';
import { commands, send, windowHeight, windowWidth, wireWindow, type Host } from '../popover/wiring';
import { Popover, PopoverFallback } from '../screens/Popover';

// The popover entry's link to the Tauri app (desktop/src-tauri/src/lib.rs). The daemon is
// cross-origin to the webview, so the app pushes it: the cached `get_snapshot` on load and on
// every re-show, then a `snapshot` event on each poll. `withGlobalTauri` puts the API on window.

interface TauriGlobal {
  core: { invoke: Invoke };
  event: { listen: NonNullable<Host['listen']> };
}

const tauri = (): TauriGlobal | undefined => (window as unknown as { __TAURI__?: TauriGlobal }).__TAURI__;

const invoke: Invoke = (cmd, args) => {
  const t = tauri();
  return t ? t.core.invoke(cmd, args) : Promise.reject(new Error('not running in the faber app'));
};

/** Sizes the window to the card and any open menu: the card's width (308 or 620 px) and height. */
function useWindowSize() {
  useEffect(() => {
    let last = '';
    let frame = 0;
    const measure = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const card = document.querySelector('section');
        if (!card) return;
        const menu = document.querySelector('[role="menu"]');
        const box = card.getBoundingClientRect();
        const width = windowWidth(box.width);
        const height = windowHeight(box.bottom, menu ? menu.getBoundingClientRect().bottom : null);
        if (`${width}x${height}` !== last) {
          last = `${width}x${height}`;
          send(invoke, commands.resize(width, height)).catch(() => {});
        }
      });
    };
    const resize = new ResizeObserver(measure);
    resize.observe(document.body);
    // A popup mounts in a portal outside the card, and the card itself can be replaced (the
    // fallback): watch the body for both.
    const mutations = new MutationObserver(measure);
    mutations.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['style', 'hidden'] });
    measure();
    return () => {
      cancelAnimationFrame(frame);
      resize.disconnect();
      mutations.disconnect();
    };
  }, []);
}

interface BoundaryProps {
  /** Changes with every snapshot: a render error is retried on the next one. */
  resetKey: number;
  children: ReactNode;
}

/**
 * Keeps one bad snapshot from blanking the popover until a restart: a render error shows the
 * fallback (Open panel, Quit), and the next snapshot tries the screen again.
 */
class Boundary extends Component<BoundaryProps, { failed: boolean; key: number }> {
  state = { failed: false, key: this.props.resetKey };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  static getDerivedStateFromProps(props: BoundaryProps, state: { failed: boolean; key: number }) {
    return props.resetKey !== state.key ? { failed: false, key: props.resetKey } : null;
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error('popover render failed', error, info.componentStack);
  }

  render() {
    return this.state.failed ? <PopoverFallback invoke={invoke} /> : this.props.children;
  }
}

/** The popover window: snapshots in, commands out, ⌘⇧F and ⌘Q, and the menu shut on blur. */
export function PopoverApp() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const [tick, setTick] = useState(0);
  const [menuOpen, setMenuOpen] = useState(false);
  // One map for the window's life, filled as snapshots arrive (it is never replaced).
  const [seen] = useState(() => new Map<number, Inflight>());

  const take = useCallback(
    (next: Snapshot) => {
      const at = Date.now();
      try {
        if (next.online && next.state) rememberLive(seen, next.state, at);
      } catch (e) {
        console.error('could not read the snapshot’s jobs', e);
      }
      setNow(at);
      setSnap(next);
      setTick((t) => t + 1);
    },
    [seen],
  );

  useEffect(() => wireWindow(window, { invoke, listen: tauri()?.event.listen }, take, () => setMenuOpen(false)), [take]);

  useWindowSize();

  return (
    <Boundary resetKey={tick}>
      <Popover snap={snap} now={now} seen={seen} invoke={invoke} onSnapshot={take} menuOpen={menuOpen} onMenuOpenChange={setMenuOpen} />
    </Boundary>
  );
}
