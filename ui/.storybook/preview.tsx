import '../src/theme/theme.css';
import type { Decorator, Preview } from '@storybook/react-vite';
import { useLayoutEffect, useRef, type ReactNode } from 'react';
import { TooltipProvider } from '../src/kit';
import { StillMotion } from '../src/popover/motion';
import { forceScheme, forceStates, markForced, scaleText, type ForcedState } from './force';

interface SurfaceProps {
  scheme: 'system' | 'light' | 'dark';
  textSize: number;
  force: Partial<Record<ForcedState, string>>;
  /** Holds pips still: no halo and no fade (see StillMotion). */
  still: boolean;
  children: ReactNode;
}

/** Every story renders as an entry does: inside the shared TooltipProvider, on the page colour. */
function Surface({ scheme, textSize, force, still, children }: SurfaceProps) {
  const root = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    forceScheme(scheme);
    scaleText(textSize);
  }, [scheme, textSize]);
  useLayoutEffect(() => {
    forceStates();
    if (root.current) markForced(root.current, force);
  }, [force]);
  return (
    <StillMotion.Provider value={still}>
      <TooltipProvider>
        <div ref={root} className="flex flex-wrap items-start gap-lg">
          {children}
        </div>
      </TooltipProvider>
    </StillMotion.Provider>
  );
}

const surface: Decorator = (Story, { globals, parameters }) => (
  <Surface
    scheme={globals.scheme ?? 'system'}
    textSize={Number(globals.textSize ?? 100)}
    force={parameters.force ?? {}}
    still={Boolean(parameters.still)}
  >
    <Story />
  </Surface>
);

const preview: Preview = {
  decorators: [surface],
  parameters: {
    // Centred, so a popup has room to open on any side of its trigger.
    layout: 'centered',
    backgrounds: { disable: true },
    controls: { disable: true },
    actions: { disable: true },
  },
  globalTypes: {
    scheme: {
      description: 'Colour scheme: the system setting, or one forced',
      toolbar: {
        title: 'Theme',
        icon: 'mirror',
        items: [
          { value: 'system', title: 'System' },
          { value: 'light', title: 'Light' },
          { value: 'dark', title: 'Dark' },
        ],
        dynamicTitle: true,
      },
    },
    textSize: {
      description: 'Text size: every type step scaled, controls kept at their height',
      toolbar: {
        title: 'Text size',
        icon: 'zoom',
        items: [
          { value: '100', title: 'Text 100%' },
          { value: '150', title: 'Text 150%' },
          { value: '200', title: 'Text 200%' },
        ],
        dynamicTitle: true,
      },
    },
  },
  initialGlobals: { scheme: 'system', textSize: '100' },
};

export default preview;
