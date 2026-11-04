/**
 * The 1-based numbers of the lines of `text` that are not in `loaded`: a longest-common-subsequence
 * diff by line, so an inserted line marks only itself and the lines below it keep their place.
 * A deleted line has no line in `text` to mark.
 */
export function changedLines(text: string, loaded: string): number[] {
  const a = text.split('\n');
  const b = loaded.split('\n');
  const n = a.length;
  const m = b.length;
  const lcs: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const out: number[] = [];
  let i = 0;
  let j = 0;
  while (i < n) {
    if (j < m && a[i] === b[j]) {
      i++;
      j++;
    } else if (j < m && lcs[i][j + 1] >= lcs[i + 1][j]) {
      j++;
    } else {
      out.push(i + 1);
      i++;
    }
  }
  return out;
}
