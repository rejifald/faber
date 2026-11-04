// What a story browser cannot ask the browser for, rewritten in the page's own stylesheets:
//
// - the colour scheme. faber's dark roles live under `@media (prefers-color-scheme: dark)` in
//   desktop/src/tokens.css, and no script can change what that query matches. So the theme switch
//   rewrites each such query's condition instead: `all` to force it on, `not all` to force it off,
//   and back to the original for System. tokens.css stays the one source of both palettes.
// - hover, pressed and keyboard focus. No script can make `:hover`, `:active` or `:focus-visible`
//   match either. So every rule that uses one gets a twin with the pseudo-class swapped for an
//   attribute on the same element (`.x[data-force~="hover"]` beside `.x:hover`), and a story's
//   `parameters.force` names the one element a pointer or the keyboard would be on.
// - a reader's larger text. The type steps are px, so the browser's own text size never reaches
//   them; the text-size switch scales each `--text-*` step instead. Controls grow with it, since
//   a ladder height is a minimum and it and the icon are set in `em` of the label (ADR-0039 § 1).

/** A pseudo-class a story can force on, by the state name the docstrings use. */
export type ForcedState = 'hover' | 'pressed' | 'focus-visible';

const PSEUDO: Record<ForcedState, string> = {
  hover: ':hover',
  pressed: ':active',
  'focus-visible': ':focus-visible',
};

const SCHEME_QUERY = /prefers-color-scheme:\s*(dark|light)/;
const original = new WeakMap<CSSMediaRule, string>();
const twinned = new WeakSet<CSSRule>();

function* rules(list: CSSRuleList): Generator<CSSRule> {
  for (const rule of Array.from(list)) {
    yield rule;
    if ('cssRules' in rule) yield* rules((rule as CSSGroupingRule).cssRules);
  }
}

function* allRules(): Generator<CSSRule> {
  for (const sheet of Array.from(document.styleSheets)) {
    let list: CSSRuleList;
    try {
      list = sheet.cssRules;
    } catch {
      continue; // a cross-origin sheet: none of faber's
    }
    yield* rules(list);
  }
}

/** Forces every `prefers-color-scheme` query to the chosen scheme, or restores them all. */
export function forceScheme(scheme: 'system' | 'light' | 'dark') {
  for (const rule of allRules()) {
    if (!(rule instanceof CSSMediaRule)) continue;
    const condition = original.get(rule) ?? rule.media.mediaText;
    const match = SCHEME_QUERY.exec(condition);
    if (!match) continue;
    original.set(rule, condition);
    rule.media.mediaText = scheme === 'system' ? condition : match[1] === scheme ? 'all' : 'not all';
  }
  // Native parts (scrollbars, the caret) follow `color-scheme`, which tokens.css sets to both.
  document.documentElement.style.colorScheme = scheme === 'system' ? '' : scheme;
}

/** The twin of one selector for a forced state, or null when it does not use that pseudo-class. */
export function twinSelector(selector: string, state: ForcedState): string | null {
  // The pseudo-class itself, not the same letters inside an escaped class name: `.hover\:x:hover`.
  const pseudo = new RegExp(`(?<!\\\\)${PSEUDO[state]}(?![\\w-])`, 'g');
  const parts = selector.split(',').filter((part) => part.search(pseudo) !== -1);
  if (parts.length === 0) return null;
  return parts.map((part) => part.trim().replace(pseudo, `[data-force~="${state}"]`)).join(', ');
}

/**
 * Marks the elements a story forces, `{ hover: '[role="tab"]:not([data-active])' }`: the first
 * match of each selector inside `root`, since a pointer is over one control at a time.
 */
export function markForced(root: HTMLElement, force: Partial<Record<ForcedState, string>>) {
  for (const marked of Array.from(root.querySelectorAll('[data-force]'))) marked.removeAttribute('data-force');
  for (const [state, selector] of Object.entries(force)) {
    const target = root.querySelector(selector);
    if (!target) throw new Error(`parameters.force.${state}: nothing matches ${selector}`);
    const states = new Set((target.getAttribute('data-force') ?? '').split(' ').filter(Boolean));
    target.setAttribute('data-force', [...states, state].join(' '));
  }
}

/** Adds a forced-state twin beside every rule that uses `:hover`, `:active` or `:focus-visible`. */
export function forceStates() {
  for (const rule of Array.from(allRules())) {
    if (!(rule instanceof CSSStyleRule) || twinned.has(rule)) continue;
    twinned.add(rule);
    const parent = (rule.parentRule ?? rule.parentStyleSheet) as CSSGroupingRule | CSSStyleSheet | null;
    if (!parent) continue;
    const siblings = Array.from(parent.cssRules);
    const body = rule.cssText.slice(rule.cssText.indexOf('{'));
    for (const state of Object.keys(PSEUDO) as ForcedState[]) {
      const twin = twinSelector(rule.selectorText, state);
      if (!twin) continue;
      const at = parent.insertRule(`${twin} ${body}`, siblings.indexOf(rule) + 1);
      twinned.add(parent.cssRules[at]);
    }
  }
}

/** The theme's type steps (`--text-body: 13px`), read from its `:root` rule. */
function typeSteps(): Map<string, number> {
  const steps = new Map<string, number>();
  for (const rule of allRules()) {
    if (!(rule instanceof CSSStyleRule) || !rule.selectorText.includes(':root')) continue;
    for (const name of Array.from(rule.style)) {
      const px = /^(\d+(?:\.\d+)?)px$/.exec(rule.style.getPropertyValue(name).trim());
      // `--text-body`, not its `--text-body--line-height` companion.
      if (px && /^--text-[a-z]+$/.test(name)) steps.set(name, Number(px[1]));
    }
  }
  return steps;
}

/** Scales every type step to `percent` of its size, or restores them at 100. */
export function scaleText(percent: number) {
  const root = document.documentElement.style;
  for (const [name, px] of typeSteps()) {
    if (percent === 100) root.removeProperty(name);
    else root.setProperty(name, `${(px * percent) / 100}px`);
  }
}
