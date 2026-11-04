import { Field as BaseField } from '@base-ui/react/field';
import { useRef, type KeyboardEvent, type Ref, type UIEvent } from 'react';
import { cx, focusRing } from './ladder';

export interface TextAreaProps {
  /** The accessible name, always set; the label is not drawn. */
  label: string;
  /** The current text (controlled). */
  value: string;
  /** Called with the new text on every edit. */
  onValueChange: (value: string) => void;
  /** Marks the box invalid: the red border. */
  invalid?: boolean;
  /** How many lines tall it is. */
  rows?: number;
  /** No border, radius or fill: the box sits inside a frame that draws them (the raw editor's). */
  bare?: boolean;
  /** Called when the box scrolls, so a gutter or overlay beside it can follow. */
  onScroll?: (event: UIEvent<HTMLTextAreaElement>) => void;
  /** The element, for a parent that syncs scroll. */
  ref?: Ref<HTMLTextAreaElement>;
}

/**
 * A multi-line text box in the mono face: no wrapping, no spellcheck, Tab inserts two spaces.
 *
 * @useWhen a whole file or a block of text is edited as text (config.toml). For one line use Input.
 * @states default, focus-visible, invalid
 */
export function TextArea({ label, value, onValueChange, invalid = false, rows = 6, bare = false, onScroll, ref }: TextAreaProps) {
  const own = useRef<HTMLTextAreaElement>(null);
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== 'Tab' || e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
    e.preventDefault();
    const el = e.currentTarget;
    const { selectionStart: a, selectionEnd: b } = el;
    onValueChange(`${value.slice(0, a)}  ${value.slice(b)}`);
    requestAnimationFrame(() => el.setSelectionRange(a + 2, a + 2));
  };
  return (
    <BaseField.Root invalid={invalid} className="flex">
      <textarea
        ref={(node) => {
          own.current = node;
          if (typeof ref === 'function') ref(node);
          else if (ref) ref.current = node;
        }}
        aria-label={label}
        aria-invalid={invalid || undefined}
        value={value}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        wrap="off"
        rows={rows}
        onChange={(e) => onValueChange(e.target.value)}
        onKeyDown={onKeyDown}
        onScroll={onScroll}
        className={cx(
          'w-full resize-none overflow-x-auto font-mono text-small leading-code text-fg whitespace-pre placeholder:text-faint',
          bare ? 'overflow-y-hidden bg-transparent px-md py-0 outline-hidden' : 'overflow-y-auto rounded-sm border border-border-control bg-surface px-md py-0',
          !bare && invalid && 'border-red',
          !bare && focusRing,
        )}
      />
    </BaseField.Root>
  );
}
