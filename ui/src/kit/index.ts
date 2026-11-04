// faber's kit (ADR-0042 § 2): every interactive part, wrapped once. Screens
// import from here, never from @base-ui/react, and never render a raw
// `<button>`. What each part is for is in docs/design/catalog.md.

export { Button, type ButtonProps, type ButtonVariant } from './Button';
export { Checkbox, type CheckboxProps } from './Checkbox';
export { Disclosure, type DisclosureProps } from './Disclosure';
export { Field, Input, type FieldProps, type InputProps } from './Field';
export { Icon, type IconProps } from './Icon';
export { IconButton, type IconButtonProps } from './IconButton';
export type { Size } from './ladder';
// The shared keyboard focus ring, for a block that draws its own link or row element.
export { focusRing } from './ladder';
export { Mark, type MarkProps } from './Mark';
export {
  Menu,
  MenuCheckboxItem,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  type MenuCheckboxItemProps,
  type MenuItemProps,
  type MenuLabelProps,
  type MenuProps,
} from './Menu';
export { Select, type SelectOption, type SelectProps } from './Select';
export { Tabs, type TabItem, type TabsProps } from './Tabs';
export { Tooltip, TooltipProvider, type TooltipProps, type TooltipProviderProps } from './Tooltip';
export { Stepper, type StepperProps } from './Stepper';
export { Switch, type SwitchProps } from './Switch';
export { TextArea, type TextAreaProps } from './TextArea';
export { TextLink, type TextLinkProps } from './TextLink';
export { SegmentedControl, type SegmentedControlProps, type SegmentedItem } from './SegmentedControl';
export { Sparkline, type SparklineProps, type SparklineTone } from './Sparkline';
export { SplitButton, type SplitButtonItem, type SplitButtonProps } from './SplitButton';
export { StatusBadge, type StatusBadgeProps } from './StatusBadge';
export { StatusDot, type StatusDotProps, type StatusTone } from './StatusDot';
export { Popover, type PopoverProps } from './Popover';
export { Meter, type MeterProps } from './Meter';
export { ScrollArea, type ScrollAreaProps } from './ScrollArea';
export { RowButton, type RowButtonProps } from './RowButton';
export { RowDisclosure, type RowDisclosureProps } from './RowDisclosure';
export { Dialog, type DialogProps } from './Dialog';
export { PeakHistogram, type PeakHistogramProps } from './PeakHistogram';
export { PhaseBar, type PhaseBarProps, type PhaseSegment, type PhaseTone } from './PhaseBar';
export { RunStrip, type RunStripProps } from './RunStrip';
export { SampleBars, type SampleBarsProps } from './SampleBars';
export { ToggleRow } from './ToggleRow';
export type { ToggleRowProps } from './ToggleRow';
