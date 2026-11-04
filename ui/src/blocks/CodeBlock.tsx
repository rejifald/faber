export interface CodeBlockProps {
  /** The text, set as text and kept as written: line breaks stay, long lines wrap anywhere. */
  children: string;
}

/**
 * A multi-line message from the daemon or GitHub, in the mono face on a recessed panel.
 *
 * @useWhen showing a commit message, an error or a log excerpt in a row's detail. Not for a
 * single value (KeyValueList) and not for text the operator edits (RawEditor).
 * @states default, long lines
 */
export function CodeBlock({ children }: CodeBlockProps) {
  return (
    <pre className="whitespace-pre-wrap rounded-md border border-border-soft bg-bg p-md font-mono text-small text-fg wrap-anywhere">
      {children}
    </pre>
  );
}
