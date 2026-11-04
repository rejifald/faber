export interface Fact {
  /** The term, in words. Set as text; truncates when long. */
  label: string;
  /** The value. Set as text. */
  value: string;
  /** A word, not a figure: set in the body face rather than mono. */
  word?: boolean;
}

export interface FactListProps {
  /** The facts, in order. */
  facts: Fact[];
}

/**
 * The term and value lines under an open popover row: the term muted on the left, the value
 * right-aligned, figures in mono and words in the body face. Popover density: tighter than the
 * dashboard's KeyValueList.
 *
 * @useWhen inside an open GateRow or VmRow, listing what the row is made of. For the dashboard's
 * facts use KeyValueList.
 * @states default, with a word value, long label
 */
export function FactList({ facts }: FactListProps) {
  return (
    <dl className="flex flex-col gap-3xs px-xl pt-2xs text-small">
      {facts.map((f) => (
        <div key={f.label} className="flex items-baseline gap-sm">
          <dt className="min-w-0 flex-1 truncate text-muted">{f.label}</dt>
          <dd className={`shrink-0 whitespace-nowrap text-fg ${f.word ? 'text-small' : 'font-mono text-caption tabular-nums'}`}>{f.value}</dd>
        </div>
      ))}
    </dl>
  );
}
