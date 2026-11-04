import { Tabs as BaseTabs } from '@base-ui/react/tabs';
import type { ReactNode } from 'react';
import { cx, focusRing, ladder } from './ladder';

/**
 * One tab: its value, its label and the panel it shows. A tab has no disabled state: it navigates,
 * so no action is ever its sibling, and it stays live while one runs (guidelines § Controls).
 */
export interface TabItem {
  value: string;
  label: ReactNode;
  content: ReactNode;
}

export interface TabsProps {
  /** The tabs, in order. */
  tabs: TabItem[];
  /** The selected tab's value, when the screen owns it (controlled). */
  value?: string;
  /** The tab selected first, when Tabs owns the value (uncontrolled). */
  defaultValue?: string;
  /** Called with the newly selected tab's value. */
  onValueChange?: (value: string) => void;
  /** Names the tab list for assistive tech, e.g. "Dashboard sections". */
  label: string;
}

/**
 * A row of tabs on the `md` step, each showing one panel, wrapping Base UI's Tabs.
 *
 * @useWhen switching between peer views of one surface (the dashboard's sections). Not for
 * navigation between surfaces, and not for a two-way setting: use Select or Checkbox.
 * @states default, hover, selected, focus-visible
 */
export function Tabs({ tabs, value, defaultValue, onValueChange, label }: TabsProps) {
  return (
    <BaseTabs.Root
      value={value}
      defaultValue={defaultValue ?? tabs[0]?.value}
      onValueChange={(next) => onValueChange?.(String(next))}
      className="flex flex-col gap-lg"
    >
      <BaseTabs.List aria-label={label} className="flex gap-3xs self-start rounded-md bg-surface-2 p-3xs">
        {tabs.map((tab) => (
          <BaseTabs.Tab
            key={tab.value}
            value={tab.value}
            className={cx(
              'inline-flex items-center font-semibold select-none cursor-default text-muted',
              ladder.md,
              'hover:text-fg data-active:bg-surface data-active:text-fg-strong data-active:shadow-raised',
              focusRing,
            )}
          >
            {tab.label}
          </BaseTabs.Tab>
        ))}
      </BaseTabs.List>
      {tabs.map((tab) => (
        <BaseTabs.Panel key={tab.value} value={tab.value} className={cx('outline-hidden', focusRing)}>
          {tab.content}
        </BaseTabs.Panel>
      ))}
    </BaseTabs.Root>
  );
}
