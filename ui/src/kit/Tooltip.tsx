import { Tooltip as BaseTooltip } from '@base-ui/react/tooltip';
import type { ReactElement, ReactNode } from 'react';

/** The tooltip bubble, shared with the Menu trigger's own tooltip. */
export const tooltipPopup =
  'rounded-sm bg-fg-strong px-sm py-2xs text-caption text-bg shadow-floating';

export interface TooltipProps {
  /** What the tooltip says: a short phrase, never the only place an action is explained. */
  content: ReactNode;
  /** The element it describes: a kit control, which receives the trigger's props and ref. */
  children: ReactElement;
  /** Which side of the trigger it opens on. */
  side?: 'top' | 'bottom' | 'left' | 'right';
}

/**
 * A hint that appears on hover or keyboard focus, wrapping Base UI's Tooltip.
 *
 * @useWhen naming an icon-only control (IconButton does this itself), or previewing why a live
 * control cannot act now (its press says so too). Not for content the user must read or act on:
 * put that in the screen.
 * @states closed, open
 */
export function Tooltip({ content, children, side = 'top' }: TooltipProps) {
  return (
    <BaseTooltip.Root>
      <BaseTooltip.Trigger render={children} />
      <BaseTooltip.Portal>
        <BaseTooltip.Positioner side={side} sideOffset={6}>
          <BaseTooltip.Popup className={tooltipPopup}>
            {content}
          </BaseTooltip.Popup>
        </BaseTooltip.Positioner>
      </BaseTooltip.Portal>
    </BaseTooltip.Root>
  );
}

export interface TooltipProviderProps {
  /** The surface's content; every Tooltip inside shares one open delay. */
  children: ReactNode;
}

/**
 * Shares one hover delay across the tooltips of a surface, so moving along a toolbar shows the
 * next hint at once.
 *
 * @useWhen once, at the root of each entry (popover, dashboard). Screens never add another.
 * @states default
 */
export function TooltipProvider({ children }: TooltipProviderProps) {
  return <BaseTooltip.Provider delay={500}>{children}</BaseTooltip.Provider>;
}
