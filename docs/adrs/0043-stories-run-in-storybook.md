---
title: Stories run in Storybook
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0043: Stories run in Storybook

## Status

accepted (2026-10-18). It settles what [ADR-0042](0042-the-ui-is-react-with-base-ui-and-tailwind.md)
§ 8 left open, which was the story runner and story coverage.

*Amended 2026-10-18 (owner, #285):* § 3's Text size switch now shows controls growing
(ADR-0039 § 1 amended), and § 4 forces a state on one named control instead of a whole
story.

## Context

The design rule (guidelines § States) is that every state of every component has a story. The owner reviews UI
in the story browser, and stories are the inputs for visual regression. #280 shipped the kit
and a catalogue that counts stories, but not a runner, because a runner is a dependency and
the owner decides those. The catalogue's `requireStory` was off until one was chosen.

Two runners were compared against `ui/` at 72f2856 (Vite 8.3.4, React 19.3.1, Tailwind 4.3.3),
both installed with install scripts off:

| | Storybook 10.6.2 | Ladle 5.1.1 |
| --- | --- | --- |
| Maker | Chromatic Inc. (US); maintainers in the Netherlands, US, Australia, Denmark, Germany; MIT | one maintainer (Czech Republic), started at Uber; MIT |
| Last release | 2026-10-12 | 2025-11-04 |
| Vite | a peer dependency, `^5` to `^8`: builds with `ui`'s own Vite 8 | a direct dependency, `^6`: installs a second Vite (6.4.4); HMR is reported broken on Vite 7 and later |
| React 19 | supported | supported |
| Added to `node_modules` | 168 packages, +63 MB | 406 packages, +77 MB |
| Toolbar switches | custom globals in the toolbar | light/dark built in, no custom toolbar |
| Telemetry | on by default | none |

Both built a story with the cleared Tailwind theme, and faber's utilities were present.

On origin, using ADR-0042's scope (direct dependencies and anything shipped are checked;
transitive build-time tools are allowed): neither direct package is of Russian origin.
Storybook adds no package of Russian origin beyond the `postcss`, `nanoid` and `browserslist`
ADR-0042 already lists, though its prebundled CLI contains `fast-glob`, which is build-time
only. Ladle would add `fast-glob` and `css-selector-parser` as transitive build-time packages.
Nothing from either runner ships: stories are not part of the dashboard or popover builds.

## Decision

1. **Storybook 10 is the story runner.** The owner chose it on 2026-10-18. `ui/` adds two
   dev dependencies, `storybook` and `@storybook/react-vite`, pinned exactly. No addon:
   the toolbar and the play-function helpers (`storybook/test`) come with `storybook`.
   `@storybook/addon-themes` is not needed (§ 3).
2. **It sends nothing.** `ui/.storybook/main.ts` turns off telemetry, crash reports and the
   what's-new feed.
3. **Every story renders as an entry does,** inside the shared `TooltipProvider` and on the
   theme, with two toolbar switches:
   - **Theme:** System, Light or Dark. faber's dark roles live only under
     `@media (prefers-color-scheme: dark)` in `desktop/src/tokens.css`. No script can change
     what that query matches, so the switch rewrites each such query's condition in the page's
     stylesheets. `tokens.css` stays the one source of both palettes, with no story-only selector.
   - **Text size:** 100, 150 or 200 %. The type steps are px, so a browser's text size never
     reaches them. The switch scales each `--text-*` step. Controls grow with it, since a
     ladder height is a minimum and its icon scales with the label (ADR-0039 § 1, amended
     2026-10-18).
4. **One story per state.** Each kit part's `@states` docstring lists its states, and its
   story file has a story for each one, plus one per ladder size. A browser cannot be made to
   hover, press or focus by a script, so each rule using `:hover`, `:active` or
   `:focus-visible` gets a twin with the pseudo-class swapped for a `data-force` attribute on
   the same element. A story's `parameters.force` names the one element a pointer or the
   keyboard would be on, never a whole row (owner 2026-10-18). Open popups (menu, select,
   tooltip) are reached by a play function, with native events.
5. **Coverage is enforced.** `requireStory` is on for every catalogue group, so
   `npm run catalog:check` fails on an entry without a story. `npm test` renders every story,
   and CI builds the static Storybook.

## Consequences

- Reviewing UI means running `npm run storybook` in `ui/`. A design PR's states are reviewed
  there, not only in screenshots.
- `ui/node_modules` grows by about 63 MB, and CI's `ui` job gains a Storybook build.
- A new kit part, block or screen needs its story file in the same change, or CI fails.
- The forced states are an emulation, made by rewriting the page's own stylesheets. A rule
  that styles hover by script rather than CSS would not show in a hover story.
- Visual regression is not part of this decision. The stories are its inputs when it comes.

## Considered and not adopted

- **Ladle:** smaller output (0.7 MB against 8.6 MB for one story). But its last release
  pins Vite 6, so it runs a second Vite beside `ui`'s, and it has one maintainer. It also has
  no custom toolbar, so the text-size switch would not fit.
- **`storybook-addon-pseudo-states` for the forced states:** it does the same stylesheet
  rewrite. It would be a third dependency for about forty lines in `ui/.storybook/force.ts`.
- **A `[data-theme]` selector in `tokens.css` for the theme switch:** it would be a second
  way to choose the dark palette, and it exists only for stories.
