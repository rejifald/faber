/**
 * The spaced pipe between two values that stand apart (`site | Deploy`, `faber v0.3.13 | app
 * v0.3.12`), in quieter ink and hidden from screen readers, who hear the values as a list.
 *
 * @useWhen between two independent values on one line. Never between a label and its count, and
 * never `·`: one phrase, or two lines (guidelines § Copy).
 * @states default
 */
export function Pipe() {
  return (
    <span aria-hidden className="px-2xs font-normal text-faint">
      |
    </span>
  );
}
