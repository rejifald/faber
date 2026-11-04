// Writes the SVG masters in this folder from the mark geometry (see README.md).
// Usage: node design/mark/generate.mjs design/mark
import { writeFileSync } from 'node:fs';

const E = 29, C = 0.5, M = 3;
const DD = M / (E / Math.sqrt(1.25)); // param step for a diagonal-running cut
const DV = M / E; // param step for a vertical-running cut
const P = (x, y, z) => [50 + E * (x - y), 50 + E * ((x + y) / 2 - z)];
const f = (n) => +n.toFixed(3);
const pts = (a) => a.map(([x, y]) => `${f(x)},${f(y)}`).join(' ');

// Shell faces, each an L: the full face minus the core and its moat.
const a = C - DD, b = C - DV;
const top = [[0, 0], [1, 0], [1, a], [a, a], [a, 1], [0, 1]].map(([x, y]) => P(x, y, 1));
const right = [[0, 0], [1, 0], [1, a], [b, a], [b, 1], [0, 1]].map(([y, z]) => P(1, y, z));
const left = [[0, 0], [1, 0], [1, a], [b, a], [b, 1], [0, 1]].map(([x, z]) => P(x, 1, z));
const ctop = [P(C, C, 1), P(1, C, 1), P(1, 1, 1), P(C, 1, 1)];
const cright = [P(1, C, C), P(1, 1, C), P(1, 1, 1), P(1, C, 1)];
const cleft = [P(C, 1, C), P(1, 1, C), P(1, 1, 1), P(C, 1, 1)];
const geo = { top, left, right, ctop, cleft, cright };

function mark({ ink, shellA, core }) {
  const s = (p, a) => `<polygon points="${pts(p)}" fill="${ink}"${a === 1 ? '' : ` fill-opacity="${a}"`}/>`;
  const c = (p, col) => `<polygon points="${pts(p)}" fill="${col}"/>`;
  return s(top, shellA[0]) + s(left, shellA[1]) + s(right, shellA[2]) +
    (Array.isArray(core)
      ? c(ctop, core[0]) + c(cleft, core[1]) + c(cright, core[2])
      : s(ctop, core.a[0]) + s(cleft, core.a[1]) + s(cright, core.a[2]));
}

const ways = {
  forge: { tile: ['#2B211C', '#15100D'], ink: '#EFE4D6', shellA: [0.95, 0.7, 0.45], core: ['#F7955A', '#E46B2B', '#B9521C'] },
  ink: { tile: ['#1E2025', '#090A0C'], ink: '#F4F4F2', shellA: [0.95, 0.7, 0.45], core: ['#F08A50', '#E0662A', '#B44E1C'] },
  porcelain: { tile: ['#FCFBF8', '#E9E5DD'], ink: '#141519', shellA: [0.4, 0.65, 0.9], core: ['#E57038', '#CC5622', '#A0421A'], hairline: true },
};
function tile(name, w, flat) {
  const [t0, t1] = w.tile;
  const defs = flat ? '' : `<defs>
<linearGradient id="${name}-tile" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${t0}"/><stop offset="1" stop-color="${t1}"/></linearGradient>
<linearGradient id="${name}-rim" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fff" stop-opacity="${w.hairline ? 0.9 : 0.16}"/><stop offset="0.45" stop-color="#fff" stop-opacity="0"/></linearGradient>
</defs>`;
  const bg = flat ? `<rect width="100" height="100" rx="24" fill="${t1}"/>`
    : `<rect width="100" height="100" rx="24" fill="url(#${name}-tile)"/><rect x="0.5" y="0.5" width="99" height="99" rx="23.5" fill="none" stroke="url(#${name}-rim)"/>` +
      (w.hairline ? `<rect x="0.25" y="0.25" width="99.5" height="99.5" rx="23.75" fill="none" stroke="#000" stroke-opacity="0.08" stroke-width="0.5"/>` : '');
  return { defs, body: bg + mark(w) };
}
const svg = (vb, inner) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="${vb}" role="img" aria-label="faber">${inner}</svg>\n`;
const out = process.argv[2] ?? new URL(".", import.meta.url).pathname;
for (const [name, w] of Object.entries(ways)) {
  const t = tile(name, w, false);
  writeFileSync(`${out}/faber-mark-${name}.svg`, svg('0 0 100 100', t.defs + t.body));
}
{
  const t = tile('forge', ways.forge, true);
  writeFileSync(`${out}/faber-mark-forge-flat.svg`, svg('0 0 100 100', t.body));
  const g = tile('forge', ways.forge, false);
  writeFileSync(`${out}/faber-app-icon-macos.svg`, svg('0 0 100 100', g.defs + `<g transform="translate(9.765 9.765) scale(0.8047)">${g.body}</g>`));
}
writeFileSync(`${out}/faber-glyph-mono.svg`, svg('20 20 60 60',
  mark({ ink: '#000', shellA: [0.38, 0.55, 0.24], core: { a: [0.78, 1, 0.56] } })));
console.log(JSON.stringify({ DD: f(DD), DV: f(DV), ...Object.fromEntries(Object.entries(geo).map(([k, v]) => [k, pts(v)])) }, null, 1));
