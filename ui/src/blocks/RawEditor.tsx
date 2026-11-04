import { TextArea } from '../kit';
import { changedLines } from '../settings/raw';

const MIN_ROWS = 12;

export interface RawError {
  line: number;
  column: number;
  message: string;
}

export interface RawEditorProps {
  /** The file's text as edited. */
  text: string;
  /** The file's text as loaded, to mark the changed lines. */
  loaded: string;
  /** The path of the file, in mono under the hint. */
  path?: string;
  /** A TOML error: its line is tinted red, the frame turns red and "Line N: message" shows. */
  error?: RawError;
  /** Called with the whole text on every edit. */
  onChange: (text: string) => void;
}

/**
 * config.toml as text: a line-number gutter, the text in mono, changed lines tinted and an error
 * line marked in red. No highlighting.
 *
 * @useWhen the operator edits the whole file as text, the last item of the Settings nav.
 * @states clean, changed, TOML error
 */
export function RawEditor({ text, loaded, path, error, onChange }: RawEditorProps) {
  const lines = text.split('\n');
  const shown = Array.from({ length: Math.max(lines.length, MIN_ROWS) });
  const changed = new Set(changedLines(text, loaded));
  const rows = Math.max(lines.length, MIN_ROWS);
  return (
    <div className="flex flex-col gap-sm">
      <header className="flex flex-col gap-2xs">
        <h2 className="text-title font-semibold text-fg-strong">config.toml</h2>
        <p className="text-small text-muted">
          The whole file, as it is on disk. Saving keeps your comments and layout. faber checks it as it checks the form: a change that widens trust is refused with its sudo command.
        </p>
        {path && <p className="font-mono text-small text-muted">{path}</p>}
      </header>
      <div className={`flex overflow-hidden rounded-sm border bg-surface outline-hidden focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-accent-text focus-within:outline-solid ${error ? 'border-red' : 'border-border-control'}`}>
        <div aria-hidden className="shrink-0 select-none bg-surface-2 px-md text-right font-mono text-small leading-code text-muted">
          {shown.map((_, i) => (
            <div key={i}>{i + 1}</div>
          ))}
        </div>
        <div className="relative min-w-0 flex-1">
          <div aria-hidden className="pointer-events-none absolute inset-x-0 top-0 font-mono text-small leading-code">
            {shown.map((_, i) => (
              <div key={i} className={error?.line === i + 1 ? 'bg-red-soft' : changed.has(i + 1) ? 'bg-accent-soft' : ''}>
                {' '}
              </div>
            ))}
          </div>
          <div className="relative">
            <TextArea bare label="config.toml" value={text} rows={rows} onValueChange={onChange} invalid={Boolean(error)} />
          </div>
        </div>
      </div>
      {error && (
        <p role="status" className="text-small text-red">
          Line {error.line}: {error.message}
        </p>
      )}
    </div>
  );
}
