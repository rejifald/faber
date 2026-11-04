/**
 * ESLint flat-config plugin `design-tokens`: keeps faber's UI on its theme (ADR-0042 § 8).
 *
 * Copied from a design-token lint pack and adapted to Tailwind: named colours
 * and colours inside longer values are caught, `no-arbitrary-class` is faber's addition, and the
 * messages name faber's guidelines.
 *
 * `no-restricted-syntax` cannot test a style property key against the type of its value, so these
 * rules live in a plugin. The import and element rules (Base UI and raw controls only in the kit,
 * no `style` prop outside it) are plain config in ui/eslint.config.mjs.
 *
 * Works on string literals, template chunks, style objects and colour attributes in TSX. It does
 * not lint CSS files: ui/src/theme/ holds the only CSS, and tests/design_tokens.rs covers the
 * token file it imports.
 *
 * Rules
 *   design-tokens/no-raw-colour        hex and functional colour literals
 *   design-tokens/no-raw-style-number  raw sizes in style objects, fontFamily string literals
 *   design-tokens/no-arbitrary-class   Tailwind arbitrary values, properties and variables
 *   design-tokens/ladder-size          control-* and icon-* sizes outside the kit, or as spacing
 *   design-tokens/no-dot-separator     a middle dot or bullet between facts in copy
 *
 * Usage:
 *   import designTokens from './eslint-rules/design-tokens.mjs';
 *   { plugins: { 'design-tokens': designTokens },
 *     rules: { 'design-tokens/no-raw-colour': 'error', 'design-tokens/no-raw-style-number': 'error' } }
 */

// Hex colours. Where a colour is expected (a colour style key, an SVG colour attribute, see
// colourContext below) any #hex is one: '0 0 #000', '#000 url(x.png)'. Elsewhere a string that
// is only a hex ('#000') is a colour, and inside longer text a hex is one when it has a letter
// a-f ('1px solid #ccc') or sits next to CSS: a length or number, a border or shadow keyword, or
// a function's paren or comma ('0 0 0 1px #000', 'gradient(#000, #111)'). '#280' or '#1234' in
// copy, even 'Run #1234 failed at 10%', is an issue or run number. A fragment spelled in hex
// letters ('#add') still looks like a colour: list it in the rule's `allow`.
const HEX_DIGITS = '(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})';
const HEX_WHOLE = new RegExp(`^#${HEX_DIGITS}$`, 'i');
const HEX_IN = new RegExp(`(^|[\\s(,:])#(${HEX_DIGITS})(?![\\w-])`, 'gi');
const CSS_NEIGHBOUR =
  /^(?:-?\d*\.?\d+(?:px|rem|em|%)?|solid|dashed|dotted|double|groove|ridge|inset|outset|none|thin|medium|thick|url\(.*)$/i;
const hasHexColour = (value, colourContext) => {
  if (HEX_WHOLE.test(value.trim())) return true;
  for (const m of value.matchAll(HEX_IN)) {
    const [whole, lead, digits] = m;
    if (colourContext || /[a-f]/i.test(digits) || lead === '(' || lead === ',') return true;
    const start = m.index + lead.length;
    const before = value.slice(0, start).trimEnd().split(/\s+/).pop() ?? '';
    const after = value.slice(m.index + whole.length).trimStart().split(/\s+/)[0] ?? '';
    if (before.endsWith('(') || before.endsWith(',') || CSS_NEIGHBOUR.test(before) || CSS_NEIGHBOUR.test(after.replace(/[,)]+$/, ''))) {
      return true;
    }
  }
  return false;
};

// CSS named colours. Checked only where a colour is expected (style keys and SVG colour
// attributes), since words like `red` or `tan` are ordinary elsewhere. currentColor, transparent
// and inherit are not colours to tokenise.
const NAMED_COLOURS = new Set(
  (
    'aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet ' +
    'brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan ' +
    'darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki darkmagenta ' +
    'darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen darkslateblue ' +
    'darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey ' +
    'dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray ' +
    'green greenyellow grey honeydew hotpink indianred indigo ivory khaki lavender lavenderblush ' +
    'lawngreen lemonchiffon lightblue lightcoral lightcyan lightgoldenrodyellow lightgray ' +
    'lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue lightslategray ' +
    'lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon ' +
    'mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue ' +
    'mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream mistyrose moccasin ' +
    'navajowhite navy oldlace olive olivedrab orange orangered orchid palegoldenrod palegreen ' +
    'paleturquoise palevioletred papayawhip peachpuff peru pink plum powderblue purple ' +
    'rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna ' +
    'silver skyblue slateblue slategray slategrey snow springgreen steelblue tan teal thistle ' +
    'tomato turquoise violet wheat white whitesmoke yellow yellowgreen'
  ).split(' '),
);
const COLOUR_KEY = /colou?r$|^(background|border|outline|fill|stroke|boxShadow|textShadow)/i;
const COLOUR_ATTRIBUTES = new Set(['fill', 'stroke', 'color', 'stopColor', 'floodColor', 'lightingColor']);
const hasNamedColour = (value) =>
  value
    .toLowerCase()
    .split(/[\s,()/]+/)
    .some((word) => NAMED_COLOURS.has(word));

const DEFAULT_COLOUR_FUNCTIONS = ['rgb', 'rgba'];

const DEFAULT_SIZE_KEYS = [
  'fontSize',
  'lineHeight',
  'letterSpacing',
  'borderRadius',
  'borderTopLeftRadius',
  'borderTopRightRadius',
  'borderBottomLeftRadius',
  'borderBottomRightRadius',
  'height',
  'minHeight',
  'maxHeight',
  'width',
  'minWidth',
  'maxWidth',
  'top',
  'right',
  'bottom',
  'left',
  'inset',
  'insetBlock',
  'insetInline',
  'gap',
  'rowGap',
  'columnGap',
];
const DEFAULT_SIZE_PREFIXES = ['padding', 'margin'];

function keyName(property) {
  if (property.computed) return null;
  if (property.key.type === 'Identifier') return property.key.name;
  if (property.key.type === 'Literal' && typeof property.key.value === 'string') {
    return property.key.value;
  }
  return null;
}

/** A bare number, or a negated one, that is not an allowed value (default: only 0 is allowed). */
function rawNumber(node, allowed) {
  if (node.type === 'Literal' && typeof node.value === 'number') {
    return !allowed.includes(node.value);
  }
  if (node.type === 'UnaryExpression' && (node.operator === '-' || node.operator === '+')) {
    return rawNumber(node.argument, allowed);
  }
  return false;
}

/** A string such as '14px' or '1.25rem' whose unit is flagged and whose amount is not allowed. */
function rawLength(node, unitPattern, allowed) {
  if (!unitPattern || node.type !== 'Literal' || typeof node.value !== 'string') return false;
  const match = unitPattern.exec(node.value.trim());
  return Boolean(match) && !allowed.includes(Number(match[1]));
}

/**
 * no-raw-colour
 *
 * Rejects: hex colours ('#fff', '1px solid #ccc', not '#280' in copy) and functional colours ('rgb(...)') in any
 * string literal or template-literal chunk, and named colours ('red', '1px solid tomato') where a
 * colour is expected: a style key such as `color` or `borderTop`, or an SVG colour attribute.
 * Why: colour values come from the token system, as semantic roles, so themes (dark mode, brand
 * variants) change in one place. A literal colour is invisible to the theme.
 * Options (object, all optional, no defaults tied to a project):
 *   functions: string[]  functional notations to reject. Default ['rgb', 'rgba'];
 *                        add 'hsl', 'hsla', 'oklch', 'hwb', 'lab', 'lch', 'color' as needed.
 *   allow: string[]      exact literal strings that stay legal (e.g. a brand colour in an SVG).
 * Scope the rule by file path in the config: exempt the theme package itself, stories and specs.
 */
const noRawColour = {
  meta: {
    type: 'problem',
    schema: [
      {
        type: 'object',
        properties: {
          functions: { type: 'array', items: { type: 'string' } },
          allow: { type: 'array', items: { type: 'string' } },
        },
        additionalProperties: false,
      },
    ],
    messages: {
      colour:
        'No hex or functional colour literals: use a colour role from the theme, e.g. `bg-surface` (docs/design/guidelines.md, Colour).',
    },
  },
  create(context) {
    const options = context.options[0] ?? {};
    const functions = options.functions ?? DEFAULT_COLOUR_FUNCTIONS;
    const allow = new Set(options.allow ?? []);
    const functional = functions.length
      ? new RegExp(`\\b(?:${functions.join('|')})\\s*\\(`, 'i')
      : null;
    // Whether a literal or template chunk is the value of a colour style key or SVG attribute.
    const colourContext = (node) => {
      let at = node.type === 'TemplateElement' ? node.parent : node;
      if (at.parent?.type === 'JSXExpressionContainer') at = at.parent;
      const owner = at.parent;
      if (owner?.type === 'Property' && owner.value === at) return COLOUR_KEY.test(keyName(owner) ?? '');
      return (
        owner?.type === 'JSXAttribute' &&
        owner.name.type === 'JSXIdentifier' &&
        COLOUR_ATTRIBUTES.has(owner.name.name)
      );
    };
    const check = (node, value) => {
      if (typeof value !== 'string' || allow.has(value)) return;
      if (hasHexColour(value, colourContext(node)) || (functional && functional.test(value))) {
        context.report({ node, messageId: 'colour' });
      }
    };
    const checkNamed = (node) => {
      if (node?.type !== 'Literal' || typeof node.value !== 'string' || allow.has(node.value)) return;
      if (hasNamedColour(node.value)) context.report({ node, messageId: 'colour' });
    };
    return {
      Literal: (node) => check(node, node.value),
      TemplateElement: (node) => check(node, node.value.cooked),
      Property(node) {
        const name = keyName(node);
        if (name && COLOUR_KEY.test(name)) checkNamed(node.value);
      },
      JSXAttribute(node) {
        if (node.name.type === 'JSXIdentifier' && COLOUR_ATTRIBUTES.has(node.name.name)) {
          checkNamed(node.value?.type === 'JSXExpressionContainer' ? node.value.expression : node.value);
        }
      },
    };
  },
};

/**
 * no-raw-style-number
 *
 * Rejects, inside object expressions (style objects):
 *   - a raw non-zero number for a size key (fontSize, lineHeight, letterSpacing, radii, width,
 *     height, minHeight, gap, and any key starting with padding or margin);
 *   - optionally a raw length string such as '14px' or '1.5rem' (web);
 *   - a string or template literal for fontFamily.
 * Why: type, spacing and sizes come from tokens or from kit components, so the scale stays
 * consistent and one change reaches every screen. 0 stays legal.
 * Options (object, all optional):
 *   sizeKeys: string[]      exact keys to check. Default: the list above.
 *   sizePrefixes: string[]  key prefixes to check. Default ['padding', 'margin'].
 *   allowedValues: number[] raw values that stay legal. Default [0].
 *   lengthUnits: string[]   units that make a string a raw length ('px', 'rem', 'em').
 *                           Default [] (React Native); use ['px', 'rem', 'em'] on the web.
 *   checkFontFamily: boolean  reject fontFamily literals. Default true.
 */
const noRawStyleNumber = {
  meta: {
    type: 'problem',
    schema: [
      {
        type: 'object',
        properties: {
          sizeKeys: { type: 'array', items: { type: 'string' } },
          sizePrefixes: { type: 'array', items: { type: 'string' } },
          allowedValues: { type: 'array', items: { type: 'number' } },
          lengthUnits: { type: 'array', items: { type: 'string' } },
          checkFontFamily: { type: 'boolean' },
        },
        additionalProperties: false,
      },
    ],
    messages: {
      number:
        'No raw `{{key}}` value in a style object: use a theme step or a kit component (docs/design/guidelines.md). 0 is allowed.',
      family:
        'No `fontFamily` string literals: use `font-sans` or `font-mono` (docs/design/guidelines.md, Typography).',
    },
  },
  create(context) {
    const options = context.options[0] ?? {};
    const sizeKeys = new Set(options.sizeKeys ?? DEFAULT_SIZE_KEYS);
    const sizePrefixes = options.sizePrefixes ?? DEFAULT_SIZE_PREFIXES;
    const allowed = options.allowedValues ?? [0];
    const checkFontFamily = options.checkFontFamily ?? true;
    const units = options.lengthUnits ?? [];
    const unitPattern = units.length
      ? new RegExp(`^(-?\\d*\\.?\\d+)(?:${units.join('|')})$`, 'i')
      : null;
    const isSizeKey = (name) =>
      sizeKeys.has(name) || sizePrefixes.some((prefix) => name.startsWith(prefix));

    return {
      Property(node) {
        if (node.parent.type !== 'ObjectExpression') return;
        const name = keyName(node);
        if (!name) return;
        if (name === 'fontFamily') {
          if (
            checkFontFamily &&
            ((node.value.type === 'Literal' && typeof node.value.value === 'string') ||
              node.value.type === 'TemplateLiteral')
          ) {
            context.report({ node: node.value, messageId: 'family' });
          }
          return;
        }
        if (
          isSizeKey(name) &&
          (rawNumber(node.value, allowed) || rawLength(node.value, unitPattern, allowed))
        ) {
          context.report({ node: node.value, messageId: 'number', data: { key: name } });
        }
      },
    };
  },
};

/**
 * no-arbitrary-class
 *
 * Rejects, in any string or template-literal chunk, a word that is a Tailwind
 *   - arbitrary value:    `p-[7px]`, `bg-[#fff]`, `hover:w-[calc(100%-2px)]`
 *   - arbitrary property: `[mask-type:luminance]`
 *   - arbitrary variant:  `[&>svg]:size-icon-sm`
 *   - arbitrary variable: `h-(--x)`, and Tailwind 4's typed form `text-(length:--x)`
 * Why: the theme is the scale (ADR-0042 § 3). Its defaults are cleared, so only faber's steps
 * exist, and a bracket is the one way left around them. A missing step is a change to the ADR
 * that owns the scale, never a local value. Scope the rule to everything but ui/src/theme/.
 * Each word is judged on its own, so prose around a class does not hide it. The shapes are narrow
 * enough that prose (`[optional]`, `items[0]`) does not match: a value needs `-[`, a property a
 * colon inside the brackets, a variant `]:` after them.
 */
const ARBITRARY = [
  /[a-z0-9]-\[[^\]\s]+\]/, // value
  /^!?\[[a-z-]+:[^\]]+\]$/, // property
  /^!?\[[^\]\s]+\]:/, // variant
  /[a-z0-9]-\((?:[a-z-]+:)?--[\w-]+\)/, // variable, plain or typed
  /[a-z0-9]-[[(]$/, // a value split by a template expression: `p-[${x}px]`
];

const noArbitraryClass = {
  meta: {
    type: 'problem',
    schema: [],
    messages: {
      arbitrary:
        'No Tailwind arbitrary value `{{word}}`: use a theme step (`p-sm`, `bg-surface`). A missing step is a change to its ADR (docs/design/guidelines.md).',
    },
  },
  create(context) {
    const check = (node, value) => {
      if (typeof value !== 'string') return;
      const word = value.split(/\s+/).find((w) => ARBITRARY.some((re) => re.test(w)));
      if (word) context.report({ node, messageId: 'arbitrary', data: { word } });
    };
    return {
      Literal: (node) => check(node, node.value),
      TemplateElement: (node) => check(node, node.value.cooked),
    };
  },
};

/**
 * ladder-size
 *
 * The control ladder's heights and icon sizes (`control-sm`…`control-lg`, `icon-sm`…`icon-lg`)
 * live in Tailwind's spacing namespace, so `p-control-lg` or `gap-icon-sm` would compile. They are
 * sizes, not spacing (ADR-0039), and only the kit sizes a control (ADR-0042 § 4).
 * Rejects any class whose value is a ladder size, unless its utility is in `allow`.
 * Options: { allow: string[] } utilities that may take a ladder size. The kit allows
 * h, w, size, min-h, min-w, max-h, max-w; everywhere else allows none.
 */
const LADDER_VALUE = /^(?:!?-?)(.+)-(?:control|icon)-(?:sm|md|lg)$/;

const ladderSize = {
  meta: {
    type: 'problem',
    schema: [
      {
        type: 'object',
        properties: { allow: { type: 'array', items: { type: 'string' } } },
        additionalProperties: false,
      },
    ],
    messages: {
      outside:
        '`{{word}}` uses a control-ladder size: only ui/src/kit sizes controls; pick a kit part and its `size` (ADR-0039, ADR-0042 § 4).',
      spacing:
        '`{{word}}` uses a ladder size as spacing: control-* and icon-* are sizes, for h, w, size and min-/max- only (ADR-0039).',
    },
  },
  create(context) {
    const allow = new Set(context.options[0]?.allow ?? []);
    const check = (node, value) => {
      if (typeof value !== 'string') return;
      for (const word of value.split(/\s+/)) {
        const utility = LADDER_VALUE.exec(word.split(':').pop() ?? '')?.[1];
        if (!utility || allow.has(utility)) continue;
        context.report({ node, messageId: allow.size ? 'spacing' : 'outside', data: { word } });
        return;
      }
    };
    return {
      Literal: (node) => check(node, node.value),
      TemplateElement: (node) => check(node, node.value.cooked),
    };
  },
};

/**
 * no-dot-separator
 *
 * Rejects a middle dot (U+00B7) or bullet (U+2022) in any string, template chunk or JSX text.
 * Why: the separator rule (guidelines § Copy, owner 2026-10-18). A dot chain reads as
 * one undifferentiated string: write one phrase, two lines, or a spaced pipe in quieter ink
 * between values that really stand apart. Scoped to ui/src; the vanilla screens keep their dots
 * until they retire. The characters are escaped here so this file does not trip a scan for them.
 */
const DOT_SEPARATOR = /[\u00b7\u2022]/;

const noDotSeparator = {
  meta: {
    type: 'problem',
    schema: [],
    messages: {
      dot: 'No middle dot or bullet between facts: write one phrase, two lines, or a spaced pipe between independent values (guidelines § Copy).',
    },
  },
  create(context) {
    const check = (node, value) => {
      if (typeof value === 'string' && DOT_SEPARATOR.test(value)) context.report({ node, messageId: 'dot' });
    };
    return {
      Literal: (node) => check(node, node.value),
      TemplateElement: (node) => check(node, node.value.cooked),
      JSXText: (node) => check(node, node.value),
    };
  },
};

export default {
  meta: { name: 'design-tokens' },
  rules: {
    'no-raw-colour': noRawColour,
    'no-raw-style-number': noRawStyleNumber,
    'no-arbitrary-class': noArbitraryClass,
    'ladder-size': ladderSize,
    'no-dot-separator': noDotSeparator,
  },
};
